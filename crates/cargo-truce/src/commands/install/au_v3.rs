//! AU v3 build + install.
//!
//! Split into two phases:
//!
//! - [`emit_au_v3_bundle`] produces a complete, signed `{Plugin Name}.app`
//!   in `target/bundles/`. No side effects outside the repo - no sudo,
//!   no `/Applications/` writes, no DAW cache bust, no pluginkit
//!   registration. Safe for CI artifacts and dry-run inspection.
//!
//! - [`install_au_v3`] copies an existing `target/bundles/{Plugin Name}.app`
//!   to `/Applications/`, clears the AU cache, and registers the appex
//!   with pluginkit. Assumes the bundle was produced by
//!   `emit_au_v3_bundle`.
//!
//! The xcode project + framework scratch stay in `tmp_dir()` - only
//! the final signed bundle lives under `target/`. Signatures are
//! produced against the `target/bundles/` path and remain valid
//! after the install-time `sudo cp -R` because macOS bundle
//! signatures hash file contents, not paths.

use crate::templates;
use crate::util::fs_ctx;
use crate::{
    Config, MacArch, PluginDef, Res, cargo_build_for_arch, deployment_target, dirs,
    extract_team_id, is_production_identity, lipo_into, release_lib_for_target, run_silent,
    run_sudo, tmp_au_v3,
};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build a signed AU v3 `.app` bundle in `target/bundles/`.
///
/// Steps:
/// 1. Build the Rust framework dylib once per arch, then `lipo -create`
///    into the canonical `lib{stem}_au.dylib` path. When `reuse_au_artifacts`
///    is true (package mode with AU2 selected first), skip the cargo +
///    lipo redo and reuse AU2's universal dylib directly.
/// 2. Assemble the `.framework` bundle in `tmp_dir()` (install-name
///    fixup + Versions/Current symlinks + Info.plist + initial sign).
/// 3. Materialize the Xcode project from embedded templates into
///    `tmp_dir()`.
/// 4. Run `xcodebuild` - produces `TruceAUv3.app` in the build dir.
/// 5. Move the built `.app` to `target/bundles/{au3_app_name}.app/`
///    and embed the Rust framework at `Contents/Frameworks/`.
/// 6. Sign inside-out (framework → appex → app) against the
///    `target/bundles/` paths. No sudo.
///
/// After return, the bundle at `target/bundles/{au3_app_name}.app/`
/// is a complete, properly-signed AU v3 container ready to be copied
/// into `/Applications/` by [`install_au_v3`].
pub(crate) fn emit_au_v3_bundle(
    root: &Path,
    config: &Config,
    plugins: &[&PluginDef],
    archs: &[MacArch],
    reuse_au_artifacts: bool,
) -> Res {
    let sign_id = crate::application_identity();
    let team_id = extract_team_id(&sign_id);
    let dt = deployment_target();

    if team_id.is_empty() {
        // `build_and_install_au_v3` filters this out as a soft skip;
        // standalone `cargo truce build --au3` reaches this branch and
        // should fail loudly with a resolution hint.
        return Err(
            "AU v3: requires a Developer ID signing identity with a team ID. \
             Set TRUCE_SIGNING_IDENTITY in .cargo/config.toml [env] \
             (e.g., \"Developer ID Application: Your Name (TEAMID)\"). \
             Ad-hoc signing (\"-\") is not supported for AU v3 appex bundles."
                .into(),
        );
    }

    if archs.is_empty() {
        return Err("emit_au_v3_bundle: empty archs list".into());
    }

    let bundles_dir = truce_build::target_dir(root).join("bundles");
    fs_ctx::create_dir_all(&bundles_dir)?;

    for p in plugins {
        build_au_v3_for_plugin(
            root,
            config,
            p,
            archs,
            &sign_id,
            &team_id,
            &dt,
            &bundles_dir,
            reuse_au_artifacts,
        )?;
    }
    Ok(())
}

/// Drive the full per-plugin AU v3 build pipeline: framework dylib →
/// `.framework` bundle → Xcode scratch → xcodebuild → embed +
/// inside-out sign. Each step is its own helper so the orchestration
/// stays readable.
#[allow(clippy::too_many_arguments)]
fn build_au_v3_for_plugin(
    root: &Path,
    config: &Config,
    p: &PluginDef,
    archs: &[MacArch],
    sign_id: &str,
    team_id: &str,
    dt: &str,
    bundles_dir: &Path,
    reuse_au_artifacts: bool,
) -> Res {
    let fw_name = p.fw_name();
    let au_v3_root = tmp_au_v3(&p.bundle_id);
    let build_dir = au_v3_root.join("build");
    let fw_build = au_v3_root.join("fw");
    let final_app = bundles_dir.join(format!("{}.app", p.au3_app_name()));

    crate::vprintln!("Building AU v3 ({})...", p.name);

    let lipo_dst = build_rust_framework_dylib(root, p, archs, dt, reuse_au_artifacts)?;
    assemble_framework_bundle(&fw_build, &fw_name, &lipo_dst, p, config, sign_id)?;
    write_xcode_project_files(&build_dir, &fw_build, p, config, team_id, &fw_name)?;
    let xcodebuild_app = run_xcodebuild_for_plugin(&build_dir, archs, p)?;
    embed_framework_into_app(&xcodebuild_app, &final_app, &fw_build, &fw_name)?;
    stage_au_v3_icon(&final_app, p)?;
    sign_au_v3_inside_out(&final_app, &build_dir, &fw_name, sign_id)?;

    crate::vprintln!("  AU v3: {}", final_app.display());
    Ok(())
}

/// Build the Rust framework dylib once per arch, then `lipo -create`
/// into the canonical `target/release/lib{stem}_au.dylib` path.
/// Returns the lipo output path.
///
/// The framework dylib is identical to AU v2's `--features au` build -
/// AU v3's appex compiles its Swift `AudioUnitFactory` /
/// `TruceAUAudioUnit` separately via xcodebuild, and display-name
/// overrides (`au_name`) travel via `PluginInfo` rather than env vars,
/// so the dylib bytes don't depend on v2 vs v3. When `reuse_au_artifacts`
/// is true (set by `cargo truce package` after AU2 built the same archs
/// in this process), skip the cargo + lipo redo and return the existing
/// universal dylib path.
fn build_rust_framework_dylib(
    root: &Path,
    p: &PluginDef,
    archs: &[MacArch],
    dt: &str,
    reuse_au_artifacts: bool,
) -> Result<PathBuf, crate::BoxErr> {
    let lipo_dst =
        truce_build::target_dir(root).join(format!("release/lib{}_au.dylib", p.dylib_stem()));

    if reuse_au_artifacts && lipo_dst.exists() {
        crate::vprintln!("  Reusing AU2 build at {}", lipo_dst.display());
        return Ok(lipo_dst);
    }

    for &arch in archs {
        crate::vprintln!("  Building Rust framework ({})...", arch.triple());
        // Same `TRUCE_AU_PLUGIN_ID` as AU v2 so the truce-au build
        // cache is shared across the v2 and v3 paths for one plugin -
        // and so the framework dylib's cocoa-view class name matches
        // what the .component build produced. v3 doesn't itself need
        // unique class names (the .appex runs sandboxed per-process),
        // but keeping the env in lockstep with v2 avoids invalidating
        // the truce-au compile when packaging both formats.
        cargo_build_for_arch(
            &[("TRUCE_AU_PLUGIN_ID", p.bundle_id.as_str())],
            &[
                "-p",
                &p.crate_name,
                "--no-default-features",
                "--features",
                "au",
            ],
            arch,
            dt,
        )?;
        let src = release_lib_for_target(root, &p.dylib_stem(), Some(arch.triple()));
        let saved =
            release_lib_for_target(root, &format!("{}_au", p.dylib_stem()), Some(arch.triple()));
        fs_ctx::copy(&src, &saved)?;
    }
    let fw_inputs: Vec<PathBuf> = archs
        .iter()
        .map(|a| release_lib_for_target(root, &format!("{}_au", p.dylib_stem()), Some(a.triple())))
        .collect();
    lipo_into(&fw_inputs, &lipo_dst)?;
    Ok(lipo_dst)
}

/// Assemble the `.framework` bundle in `tmp_dir()`: install-name
/// fixup, Versions/Current symlinks, Info.plist, initial sign.
///
/// Preserves `fw_build` across runs so xcodebuild's link-time
/// framework metadata cache survives - we overwrite the pieces that
/// change (dylib, plist, symlinks) idempotently.
fn assemble_framework_bundle(
    fw_build: &Path,
    fw_name: &str,
    lipo_dst: &Path,
    p: &PluginDef,
    config: &Config,
    sign_id: &str,
) -> Res {
    let fw_dir = fw_build.join(format!("{fw_name}.framework/Versions/A"));
    fs_ctx::create_dir_all(fw_dir.join("Resources"))?;
    fs_ctx::copy(lipo_dst, fw_dir.join(fw_name))?;

    let status = Command::new("install_name_tool")
        .args([
            "-id",
            &format!("@rpath/{fw_name}.framework/Versions/A/{fw_name}"),
        ])
        .arg(fw_dir.join(fw_name))
        .status()?;
    if !status.success() {
        return Err("install_name_tool failed".into());
    }

    let fw_root = fw_build.join(format!("{fw_name}.framework"));
    #[cfg(unix)]
    {
        // Idempotent symlink re-creation: remove any stale link
        // first, then create fresh. Needed because we no longer
        // wipe `fw_build` between runs.
        let ensure_symlink = |target: &str, link: &Path| -> Res {
            let _ = fs::remove_file(link);
            std::os::unix::fs::symlink(target, link)?;
            Ok(())
        };
        ensure_symlink("A", &fw_root.join("Versions/Current"))?;
        ensure_symlink(
            &format!("Versions/Current/{fw_name}"),
            &fw_root.join(fw_name),
        )?;
        ensure_symlink("Versions/Current/Resources", &fw_root.join("Resources"))?;
    }
    #[cfg(not(unix))]
    {
        return Err("AU v3 framework builds are only supported on macOS".into());
    }

    fs_ctx::write_if_changed(
        fw_dir.join("Resources/Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>{fw}</string>
<key>CFBundleIdentifier</key><string>com.{vid}.{suf}.framework</string>
<key>CFBundlePackageType</key><string>FMWK</string>
<key>CFBundleVersion</key><string>1</string>
</dict></plist>"#,
            fw = fw_name,
            vid = config.vendor.id.trim_start_matches("com."),
            suf = p.bundle_id,
        ),
    )?;

    // Initial framework sign. Not strictly required (we re-sign
    // inside-out after embedding into the .app), but xcodebuild
    // reads the framework to resolve symbols and rejects unsigned
    // frameworks under `CODE_SIGN_STYLE = Manual`.
    let mut cs_args: Vec<&OsStr> = vec![
        OsStr::new("--force"),
        OsStr::new("--sign"),
        OsStr::new(sign_id),
    ];
    if is_production_identity(sign_id) {
        cs_args.extend_from_slice(&[
            OsStr::new("--options"),
            OsStr::new("runtime"),
            OsStr::new("--timestamp"),
        ]);
    }
    cs_args.push(fw_root.as_os_str());
    crate::run_codesign(&cs_args, false)?;
    Ok(())
}

/// Materialize the Xcode project scratch from embedded templates.
///
/// Preserves `build_dir` across runs so xcodebuild's `DerivedData` /
/// `SYMROOT` build cache survives. `write_if_changed` for every source
/// file means mtimes only bump when the embedded template bytes
/// actually shifted - xcodebuild then incrementally rebuilds only the
/// TUs that changed.
fn write_xcode_project_files(
    build_dir: &Path,
    fw_build: &Path,
    p: &PluginDef,
    config: &Config,
    team_id: &str,
    fw_name: &str,
) -> Res {
    fs_ctx::create_dir_all(build_dir.join("AUExt"))?;
    fs_ctx::create_dir_all(build_dir.join("App"))?;
    fs_ctx::create_dir_all(build_dir.join("XcodeAUv3.xcodeproj"))?;

    fs_ctx::write_if_changed(
        build_dir.join("AUExt/AudioUnitFactory.swift"),
        templates::au3::SWIFT_SOURCE,
    )?;
    fs_ctx::write_if_changed(
        build_dir.join("AUExt/BridgingHeader.h"),
        templates::au3::BRIDGING_HEADER,
    )?;
    fs_ctx::write_if_changed(
        build_dir.join("AUExt/au_shim_types.h"),
        templates::au3::SHIM_TYPES_H,
    )?;
    fs_ctx::write_if_changed(
        build_dir.join("AUExt/AUExt.entitlements"),
        templates::au3::APPEX_ENTITLEMENTS,
    )?;
    fs_ctx::write_if_changed(build_dir.join("App/main.m"), templates::au3::APP_MAIN_M)?;
    fs_ctx::write_if_changed(
        build_dir.join("App/App.entitlements"),
        templates::au3::APP_ENTITLEMENTS,
    )?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let ver = format!("{}.{}", now.as_secs(), now.subsec_millis());

    // `min_os` / `supported_platform` must match the pbxproj's
    // `MACOSX_DEPLOYMENT_TARGET` (`generate_pbxproj` hard-codes 13.0)
    // and the `MacOSX` platform string. Mismatches surface as the
    // xcodebuild error "embedded content built for the (null) platform"
    // when the outer `.app` tries to embed the `.appex`. `xcode_tokens`
    // is `None` because xcodebuild expands the `$(...)` plist tokens
    // from this target's `PRODUCT_BUNDLE_IDENTIFIER` etc. at build time.
    let au_name = format!(
        "{}: {}",
        config.vendor.name,
        p.au3_name.as_deref().unwrap_or(p.name.as_str()),
    );
    let plist = templates::au3::render_appex_info_plist(&templates::au3::AppexPlistValues {
        au_name: &au_name,
        au_type: p.resolved_au_type(),
        au_sub: p.au3_sub(),
        au_mfr: &config.vendor.au_manufacturer,
        au_tag: &p.au_tag,
        au_ver: &ver,
        min_os: "13.0",
        supported_platform: "MacOSX",
        xcode_tokens: None,
    });
    // AUVER regenerates every call (CFBundleVersion cache-bust for
    // hosts), so this plist's bytes shift run-to-run regardless.
    // xcodebuild still bundles the new plist but skips Swift / ObjC
    // recompilation because those sources stayed stable.
    fs_ctx::write_if_changed(build_dir.join("AUExt/Info.plist"), plist)?;

    fs_ctx::write_if_changed(
        build_dir.join("XcodeAUv3.xcodeproj/project.pbxproj"),
        generate_pbxproj(
            team_id,
            &format!("{}.v3", p.bundle_id),
            &format!("{}.v3.ext", p.bundle_id),
            build_dir.join("AUExt").to_str().unwrap(),
            fw_build.to_str().unwrap(),
            fw_name,
        ),
    )?;

    // CFBundleIconFile is only emitted when the plugin declares a
    // `macos_icon` (the file is copied into Contents/Resources after
    // xcodebuild). Without the file present, macOS scribbles a
    // missing-resource error into the system log on first launch.
    let icon_keys = if p.macos_icon.is_some() {
        "    <key>CFBundleIconFile</key>\n    <string>icon</string>\n"
    } else {
        ""
    };
    let app_plist = templates::au3::APP_INFO_PLIST.replace("APPICON_KEYS", icon_keys);
    fs_ctx::write_if_changed(build_dir.join("App/Info.plist"), app_plist)?;
    Ok(())
}

/// Run xcodebuild against the materialized project. Returns the path
/// of the produced `TruceAUv3.app`.
fn run_xcodebuild_for_plugin(
    build_dir: &Path,
    archs: &[MacArch],
    p: &PluginDef,
) -> Result<PathBuf, crate::BoxErr> {
    crate::vprintln!("  Building with xcodebuild...");
    let archs_flag = format!(
        "ARCHS={}",
        archs
            .iter()
            .map(|a| match a {
                MacArch::X86_64 => "x86_64",
                MacArch::Arm64 => "arm64",
            })
            .collect::<Vec<_>>()
            .join(" ")
    );
    let output = Command::new("xcodebuild")
        .current_dir(build_dir)
        .args([
            "-project",
            "XcodeAUv3.xcodeproj",
            "-target",
            "TruceAUv3",
            "-configuration",
            "Release",
        ])
        .arg(&archs_flag)
        .arg("ONLY_ACTIVE_ARCH=NO")
        .arg(format!("SYMROOT={}/build", build_dir.display()))
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().chain(stderr.lines()) {
            if line.contains("error:") || line.contains("BUILD FAILED") {
                eprintln!("  {line}");
            }
        }
        return Err(format!("xcodebuild failed for {}", p.name).into());
    }

    let xcodebuild_app = build_dir.join("build/Release/TruceAUv3.app");
    if !xcodebuild_app.exists() {
        return Err(format!(
            "xcodebuild reported success but app not found at {}",
            xcodebuild_app.display()
        )
        .into());
    }
    Ok(xcodebuild_app)
}

/// `ditto` the xcodebuild output into `target/bundles/` and embed
/// the Rust framework at `Contents/Frameworks/`. `ditto` preserves
/// xattrs/ACLs/resource forks better than `cp -R`; macOS code
/// signatures survive the copy cleanly.
fn embed_framework_into_app(
    xcodebuild_app: &Path,
    final_app: &Path,
    fw_build: &Path,
    fw_name: &str,
) -> Res {
    let _ = fs::remove_dir_all(final_app);
    let ditto_status = Command::new("ditto")
        .arg(xcodebuild_app)
        .arg(final_app)
        .status()?;
    if !ditto_status.success() {
        return Err(format!(
            "ditto failed copying {} → {}",
            xcodebuild_app.display(),
            final_app.display()
        )
        .into());
    }

    let frameworks_dir = final_app.join("Contents/Frameworks");
    fs_ctx::create_dir_all(&frameworks_dir)?;
    let embedded_fw = frameworks_dir.join(format!("{fw_name}.framework"));
    let _ = fs::remove_dir_all(&embedded_fw);
    let fw_src = fw_build.join(format!("{fw_name}.framework"));
    let ditto_status = Command::new("ditto")
        .arg(&fw_src)
        .arg(&embedded_fw)
        .status()?;
    if !ditto_status.success() {
        return Err("ditto failed copying framework into .app".into());
    }
    Ok(())
}

/// Drop the plugin's `macos_icon` into `Contents/Resources/icon.icns`
/// so the outer `.app` shows up in Finder / Launchpad with the same
/// art the standalone host uses. No-op when `macos_icon` is unset (the
/// matching `CFBundleIconFile` plist key is also omitted in that case,
/// so macOS falls back to the system default without logging a
/// missing-resource error). Runs before signing so the icon lands
/// inside the signed seal.
#[cfg(target_os = "macos")]
fn stage_au_v3_icon(final_app: &Path, p: &PluginDef) -> Res {
    let Some(icon_rel) = &p.macos_icon else {
        return Ok(());
    };
    let icon_src = crate::project_root().join(icon_rel);
    if !icon_src.exists() {
        return Err(format!(
            "macos_icon for `{}` points to {} but no file is there.",
            p.name,
            icon_src.display()
        )
        .into());
    }
    let resources_dir = final_app.join("Contents/Resources");
    fs_ctx::create_dir_all(&resources_dir)?;
    fs_ctx::copy(&icon_src, resources_dir.join("icon.icns"))?;
    Ok(())
}

/// Sign the assembled bundle inside-out: framework → appex → app.
///
/// Order matters: framework first (parent bundle references its
/// signature), then appex (embeds its entitlements), then app (wraps
/// everything). Re-signing an inner bundle invalidates the outer, so
/// signing in the wrong order leaves the whole thing broken.
fn sign_au_v3_inside_out(final_app: &Path, build_dir: &Path, fw_name: &str, sign_id: &str) -> Res {
    let runtime_flags: &[&OsStr] = if is_production_identity(sign_id) {
        &[
            OsStr::new("--options"),
            OsStr::new("runtime"),
            OsStr::new("--timestamp"),
        ]
    } else {
        &[]
    };

    let embedded_fw = final_app.join(format!("Contents/Frameworks/{fw_name}.framework"));
    let mut args: Vec<&OsStr> = vec![
        OsStr::new("--force"),
        OsStr::new("--sign"),
        OsStr::new(sign_id),
    ];
    args.extend_from_slice(runtime_flags);
    args.push(embedded_fw.as_os_str());
    crate::run_codesign(&args, false)?;

    let appex_path = final_app.join("Contents/PlugIns/AUExt.appex");
    let entitlements_appex = build_dir.join("AUExt/AUExt.entitlements");
    let mut args: Vec<&OsStr> = vec![
        OsStr::new("--force"),
        OsStr::new("--sign"),
        OsStr::new(sign_id),
        OsStr::new("--entitlements"),
        entitlements_appex.as_os_str(),
        OsStr::new("--generate-entitlement-der"),
    ];
    args.extend_from_slice(runtime_flags);
    args.push(appex_path.as_os_str());
    crate::run_codesign(&args, false)?;

    let entitlements_app = build_dir.join("App/App.entitlements");
    let mut args: Vec<&OsStr> = vec![
        OsStr::new("--force"),
        OsStr::new("--sign"),
        OsStr::new(sign_id),
        OsStr::new("--entitlements"),
        entitlements_app.as_os_str(),
        OsStr::new("--generate-entitlement-der"),
    ];
    args.extend_from_slice(runtime_flags);
    args.push(final_app.as_os_str());
    crate::run_codesign(&args, false)?;
    Ok(())
}

/// Install pre-built AU v3 bundles from `target/bundles/` to
/// `/Applications/` and register with pluginkit.
///
/// Expects [`emit_au_v3_bundle`] to have been called first. Batched
/// into three phases so the daemon-restart + cache-bust sequence
/// happens once per install instead of once per plugin:
///
/// 1. **Per plugin** - pre-clean stale `pluginkit` state, `sudo ditto`
///    the bundle into `/Applications/`, and `lsregister -f -R`.
/// 2. **Once for the batch** - `killall pkd` +
///    `killall AudioComponentRegistrar`, clear the AU cache, wait 2s
///    for `pkd` to respawn. Previously this ran per-plugin, wasting
///    `(N-1) × 2s` plus the daemon-respawn cost.
/// 3. **Per plugin** - `pluginkit -a` + poll-until-registered, then
///    print `Installed:`.
fn install_au_v3(root: &Path, config: &Config, plugins: &[&PluginDef]) -> Res {
    // ---- Phase 1: copy bundles + lsregister per plugin ----
    #[derive(Clone)]
    struct Staged {
        app_dir: String,
        appex_id: String,
    }
    let mut staged: Vec<Staged> = Vec::with_capacity(plugins.len());

    for p in plugins {
        let app_name = p.au3_app_name();
        let final_app = truce_build::target_dir(root)
            .join("bundles")
            .join(format!("{app_name}.app"));
        if !final_app.exists() {
            return Err(format!(
                "AU v3 bundle missing at {}. Run `cargo truce build --au3 -p {}` first.",
                final_app.display(),
                p.bundle_id,
            )
            .into());
        }

        let app_dir = format!("/Applications/{app_name}.app");
        let appex_id = format!(
            "com.{}.{}.v3.ext",
            config.vendor.id.trim_start_matches("com."),
            p.bundle_id
        );

        // Pre-clean. `pluginkit -e ignore` only disables the registration -
        // if `pkd` auto-discovered the staging-tree appex during the build
        // phase, its path stays in the database and can win the next dyld
        // load race over our installed copy. `pluginkit -r <path>` evicts
        // it so the subsequent `-a /Applications/...` registers cleanly.
        let staging_appex = final_app.join("Contents/PlugIns/AUExt.appex");
        if staging_appex.exists() {
            let _ = Command::new("pluginkit")
                .args(["-r", staging_appex.to_str().unwrap()])
                .output();
        }
        let _ = Command::new("pluginkit")
            .args(["-e", "ignore", "-i", &appex_id])
            .output();
        let _ = run_sudo("rm", &[OsStr::new("-rf"), OsStr::new(&app_dir)]);

        // Install to /Applications/. `ditto` preserves the existing
        // signature since we signed the bundle at build time.
        run_sudo("ditto", &[final_app.as_os_str(), OsStr::new(&app_dir)])?;

        // lsregister updates the LaunchServices DB; it doesn't need
        // `pkd` alive, so it can run in the per-plugin phase.
        let _ = Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
            .args(["-f", "-R", &app_dir]).output();

        staged.push(Staged { app_dir, appex_id });
    }

    // ---- Phase 2: daemon restart + cache bust, once ----
    // Hosts read AudioComponent metadata from
    // `~/Library/Caches/AudioUnitCache/` which is populated by
    // `AudioComponentRegistrar`. Killing both daemons + clearing the
    // cache forces a clean re-scan on the next host launch. The 2s
    // sleep gives `pkd` time to respawn before we call `pluginkit -a`
    // (which silently no-ops if `pkd` is mid-respawn).
    run_silent("killall", &[OsStr::new("-9"), OsStr::new("pkd")]);
    run_silent(
        "killall",
        &[OsStr::new("-9"), OsStr::new("AudioComponentRegistrar")],
    );
    if let Some(home) = dirs::home_dir() {
        let _ = fs::remove_dir_all(home.join("Library/Caches/AudioUnitCache"));
    }
    std::thread::sleep(std::time::Duration::from_secs(2));

    // ---- Phase 3: register + verify per plugin ----
    for s in &staged {
        let appex_path = format!("{}/Contents/PlugIns/AUExt.appex", s.app_dir);
        if !register_appex(&appex_path, &s.appex_id) {
            eprintln!(
                "  WARNING: pluginkit did not register {}. \
                 Run `pluginkit -a \"{}\"` manually after `pkd` has settled.",
                s.appex_id, appex_path
            );
        }
        crate::log_output(format!("AU3:  {}", s.app_dir));
    }

    Ok(())
}

/// Register an AU v3 appex and verify pluginkit actually picked it up.
/// `pluginkit -a` returns 0 even when `pkd` is down, so we poll
/// `pluginkit -m -i <bundle_id>` until the id shows up in the registry.
/// Returns true on confirmed registration.
fn register_appex(appex_path: &str, appex_id: &str) -> bool {
    for _ in 0..8 {
        let _ = Command::new("pluginkit").args(["-a", appex_path]).output();
        if let Ok(out) = Command::new("pluginkit")
            .args(["-m", "-v", "-i", appex_id])
            .output()
        {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.contains(appex_id) {
                return true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    false
}

/// Build-then-install convenience for `cargo truce install --au3`.
///
/// `no_build` skips the build phase and consumes whatever's already
/// in `target/bundles/` - mirrors the behavior of the other formats'
/// `--no-build` paths.
pub(crate) fn build_and_install_au_v3(
    root: &Path,
    config: &Config,
    plugins: &[&PluginDef],
    no_build: bool,
) -> Res {
    // Single skip gate. If we don't have a real Developer ID, don't
    // build *or* install - otherwise install_au_v3 would happily copy
    // a stale signed bundle from a previous run and produce a misleading
    // "succeeded" line in the install summary.
    let sign_id = crate::application_identity();
    if extract_team_id(&sign_id).is_empty() {
        crate::log_skip(
            "AU v3: needs a Developer ID with team ID. \
             Set TRUCE_SIGNING_IDENTITY in .cargo/config.toml [env] \
             (e.g., \"Developer ID Application: Your Name (TEAMID)\"); \
             ad-hoc signing (\"-\") is not supported for AU v3 appex bundles."
                .to_string(),
        );
        return Ok(());
    }
    if !no_build {
        // `cargo truce install` only needs the host arch - universal
        // builds are reserved for the packaging path. AU2 isn't built
        // here, so the AU3 path can't reuse an AU2 artifact.
        emit_au_v3_bundle(root, config, plugins, &[MacArch::host()], false)?;
    }
    install_au_v3(root, config, plugins)
}

// Single embedded template; splitting it would just hide the literal.
#[allow(clippy::too_many_lines)]
fn generate_pbxproj(
    team_id: &str,
    app_bundle_id: &str,
    appex_bundle_id: &str,
    shim_dir: &str,
    fw_search: &str,
    fw_name: &str,
) -> String {
    format!(
        r#"// !$*UTF8*$!
{{
	archiveVersion = 1;
	classes = {{}};
	objectVersion = 56;
	objects = {{
		AA000001 = {{isa = PBXGroup; children = (AA000010, AA000011, AA000012); name = App; sourceTree = "<group>";}};
		AA000010 = {{isa = PBXFileReference; path = "App/main.m"; sourceTree = SOURCE_ROOT;}};
		AA000011 = {{isa = PBXFileReference; path = "App/Info.plist"; sourceTree = SOURCE_ROOT;}};
		AA000012 = {{isa = PBXFileReference; path = "App/App.entitlements"; sourceTree = SOURCE_ROOT;}};
		AA000020 = {{isa = PBXBuildFile; fileRef = AA000010;}};
		BB000001 = {{isa = PBXGroup; children = (BB000010, BB000011, BB000012, BB000013); name = AUExt; sourceTree = "<group>";}};
		BB000010 = {{isa = PBXFileReference; path = "AUExt/AudioUnitFactory.swift"; sourceTree = SOURCE_ROOT;}};
		BB000011 = {{isa = PBXFileReference; path = "AUExt/Info.plist"; sourceTree = SOURCE_ROOT;}};
		BB000012 = {{isa = PBXFileReference; path = "AUExt/AUExt.entitlements"; sourceTree = SOURCE_ROOT;}};
		BB000013 = {{isa = PBXFileReference; path = "AUExt/BridgingHeader.h"; sourceTree = SOURCE_ROOT;}};
		BB000020 = {{isa = PBXBuildFile; fileRef = BB000010;}};
		CC000001 = {{isa = PBXFileReference; explicitFileType = wrapper.application; path = "TruceAUv3.app"; sourceTree = BUILT_PRODUCTS_DIR;}};
		CC000002 = {{isa = PBXFileReference; explicitFileType = "wrapper.app-extension"; path = "AUExt.appex"; sourceTree = BUILT_PRODUCTS_DIR;}};
		CC000003 = {{isa = PBXGroup; children = (CC000001, CC000002); name = Products; sourceTree = "<group>";}};
		DD000001 = {{isa = PBXBuildFile; fileRef = CC000002; settings = {{ATTRIBUTES = (RemoveHeadersOnCopy,);}}; }};
		DD000002 = {{isa = PBXCopyFilesBuildPhase; buildActionMask = 2147483647; dstPath = ""; dstSubfolderSpec = 13; files = (DD000001,); name = "Embed Extensions";}};
		EE000001 = {{isa = PBXBuildFile; fileRef = EE000010;}};
		EE000002 = {{isa = PBXBuildFile; fileRef = EE000011;}};
		EE000003 = {{isa = PBXBuildFile; fileRef = EE000012;}};
		EE000010 = {{isa = PBXFileReference; lastKnownFileType = wrapper.framework; name = AudioToolbox.framework; path = System/Library/Frameworks/AudioToolbox.framework; sourceTree = SDKROOT;}};
		EE000011 = {{isa = PBXFileReference; lastKnownFileType = wrapper.framework; name = CoreAudioKit.framework; path = System/Library/Frameworks/CoreAudioKit.framework; sourceTree = SDKROOT;}};
		EE000012 = {{isa = PBXFileReference; lastKnownFileType = wrapper.framework; name = AVFAudio.framework; path = System/Library/Frameworks/AVFAudio.framework; sourceTree = SDKROOT;}};
		EE000020 = {{isa = PBXFrameworksBuildPhase; files = (EE000001, EE000002, EE000003);}};
		FF000001 = {{isa = PBXSourcesBuildPhase; files = (AA000020,);}};
		FF000002 = {{isa = PBXSourcesBuildPhase; files = (BB000020,);}};
		GG000010 = {{isa = PBXFileReference; lastKnownFileType = wrapper.framework; name = Cocoa.framework; path = System/Library/Frameworks/Cocoa.framework; sourceTree = SDKROOT;}};
		GG000020 = {{isa = PBXBuildFile; fileRef = GG000010;}};
		FF000003 = {{isa = PBXFrameworksBuildPhase; files = (GG000020,);}};
		00000001 = {{isa = PBXGroup; children = (AA000001, BB000001, CC000003); sourceTree = "<group>";}};
		11000001 = {{
			isa = PBXNativeTarget;
			buildConfigurationList = 11000010;
			buildPhases = (FF000001, FF000003, DD000002);
			dependencies = (11000020,);
			name = TruceAUv3;
			productName = TruceAUv3;
			productReference = CC000001;
			productType = "com.apple.product-type.application";
		}};
		11000010 = {{isa = XCConfigurationList; buildConfigurations = (11000011,);}};
		11000011 = {{
			isa = XCBuildConfiguration;
			buildSettings = {{
				PRODUCT_BUNDLE_IDENTIFIER = "com.truce.{app_bundle_id}";
				PRODUCT_NAME = "$(TARGET_NAME)";
				INFOPLIST_FILE = "App/Info.plist";
				CODE_SIGN_ENTITLEMENTS = "App/App.entitlements";
				CODE_SIGN_STYLE = Manual;
				CODE_SIGN_IDENTITY = "Developer ID Application";
				DEVELOPMENT_TEAM = {team_id};
				SWIFT_VERSION = 5.0;
				MACOSX_DEPLOYMENT_TARGET = 13.0;
			}};
			name = Release;
		}};
		11000020 = {{isa = PBXTargetDependency; target = 22000001;}};
		22000001 = {{
			isa = PBXNativeTarget;
			buildConfigurationList = 22000010;
			buildPhases = (FF000002, EE000020);
			dependencies = ();
			name = AUExt;
			productName = AUExt;
			productReference = CC000002;
			productType = "com.apple.product-type.app-extension";
		}};
		22000010 = {{isa = XCConfigurationList; buildConfigurations = (22000011,);}};
		22000011 = {{
			isa = XCBuildConfiguration;
			buildSettings = {{
				PRODUCT_BUNDLE_IDENTIFIER = "com.truce.{appex_bundle_id}";
				PRODUCT_NAME = "$(TARGET_NAME)";
				INFOPLIST_FILE = "AUExt/Info.plist";
				CODE_SIGN_ENTITLEMENTS = "AUExt/AUExt.entitlements";
				CODE_SIGN_STYLE = Manual;
				CODE_SIGN_IDENTITY = "Developer ID Application";
				DEVELOPMENT_TEAM = {team_id};
				SWIFT_VERSION = 5.0;
				MACOSX_DEPLOYMENT_TARGET = 13.0;
				APPLICATION_EXTENSION_API_ONLY = YES;
				SWIFT_OBJC_BRIDGING_HEADER = "AUExt/BridgingHeader.h";
				HEADER_SEARCH_PATHS = "{shim_dir}";
				FRAMEWORK_SEARCH_PATHS = "{fw_search}";
				LD_RUNPATH_SEARCH_PATHS = "@executable_path/../../../../Frameworks";
				OTHER_LDFLAGS = ("-framework", "{fw_name}");
			}};
			name = Release;
		}};
		99000001 = {{
			isa = PBXProject;
			buildConfigurationList = 99000010;
			mainGroup = 00000001;
			productRefGroup = CC000003;
			targets = (11000001, 22000001);
		}};
		99000010 = {{isa = XCConfigurationList; buildConfigurations = (99000011,);}};
		99000011 = {{
			isa = XCBuildConfiguration;
			buildSettings = {{
				SDKROOT = macosx;
				MACOSX_DEPLOYMENT_TARGET = 13.0;
				ARCHS = arm64;
			}};
			name = Release;
		}};
	}};
	rootObject = 99000001;
}}"#,
    )
}
