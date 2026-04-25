//! AU v3 build + install.
//!
//! Split into two phases:
//!
//! - [`emit_au_v3_bundle`] produces a complete, signed `{Plugin Name}.app`
//!   in `target/bundles/`. No side effects outside the repo — no sudo,
//!   no `/Applications/` writes, no DAW cache bust, no pluginkit
//!   registration. Safe for CI artifacts and dry-run inspection.
//!
//! - [`install_au_v3`] copies an existing `target/bundles/{Plugin Name}.app`
//!   to `/Applications/`, clears the AU cache, and registers the appex
//!   with pluginkit. Assumes the bundle was produced by
//!   `emit_au_v3_bundle`.
//!
//! See `truce-docs/docs/internal/build-install-split.md` for the
//! design rationale.
//!
//! The xcode project + framework scratch stay in `tmp_dir()` — only
//! the final signed bundle lives under `target/`. Signatures are
//! produced against the `target/bundles/` path and remain valid
//! after the install-time `sudo cp -R` because macOS bundle
//! signatures hash file contents, not paths.

use crate::templates;
use crate::util::fs_ctx;
use crate::{
    cargo_build_for_arch, deployment_target, dirs, extract_team_id, is_production_identity,
    lipo_into, release_lib_for_target, run_sudo, run_sudo_silent, tmp_dir, Config, MacArch,
    PluginDef, Res,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build a signed AU v3 `.app` bundle in `target/bundles/`.
///
/// Steps:
/// 1. Build the Rust framework dylib once per arch, then `lipo -create`
///    into the canonical `lib{stem}_v3.dylib` path.
/// 2. Assemble the `.framework` bundle in `tmp_dir()` (install-name
///    fixup + Versions/Current symlinks + Info.plist + initial sign).
/// 3. Materialize the Xcode project from embedded templates into
///    `tmp_dir()`.
/// 4. Run `xcodebuild` — produces `TruceAUv3.app` in the build dir.
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
) -> Res {
    let sign_id = config.macos.application_identity();
    let team_id = extract_team_id(sign_id);
    let dt = &deployment_target();

    if team_id.is_empty() {
        // `build_and_install_au_v3` filters this out as a soft skip;
        // standalone `cargo truce build --au3` reaches this branch and
        // should fail loudly with a resolution hint.
        return Err(
            "AU v3: requires a Developer ID signing identity with a team ID. \
             Set [macos.signing].application_identity in truce.toml \
             (e.g., \"Developer ID Application: Your Name (TEAMID)\"). \
             Ad-hoc signing (\"-\") is not supported for AU v3 appex bundles."
                .into(),
        );
    }

    if archs.is_empty() {
        return Err("emit_au_v3_bundle: empty archs list".into());
    }

    let bundles_dir = root.join("target/bundles");
    fs_ctx::create_dir_all(&bundles_dir)?;

    for p in plugins {
        let fw_name = p.fw_name();
        let au_v3_sub = p.au3_sub();
        let build_dir = tmp_dir().join(format!("au_v3_build_{}", p.bundle_id));
        let fw_build = tmp_dir().join(format!("au_v3_fw_{}", p.bundle_id));
        let final_app = bundles_dir.join(format!("{}.app", p.au3_app_name()));

        crate::vprintln!("Building AU v3 ({})...", p.name);

        // --- Step 1: Rust framework dylib (per-arch + lipo) -----------------
        for &arch in archs {
            crate::vprintln!("  Building Rust framework ({})...", arch.triple());
            let mut env_pairs: Vec<(&str, &str)> = vec![
                ("TRUCE_AU_VERSION", "3"),
                ("TRUCE_AU_PLUGIN_ID", &p.bundle_id),
            ];
            if let Some(n) = p.au3_name.as_deref() {
                env_pairs.push(("TRUCE_AU_NAME_OVERRIDE", n));
            }
            cargo_build_for_arch(
                &env_pairs,
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
            let saved = release_lib_for_target(
                root,
                &format!("{}_v3", p.dylib_stem()),
                Some(arch.triple()),
            );
            fs_ctx::copy(&src, &saved)?;
        }
        let fw_inputs: Vec<PathBuf> = archs
            .iter()
            .map(|a| {
                release_lib_for_target(root, &format!("{}_v3", p.dylib_stem()), Some(a.triple()))
            })
            .collect();
        let lipo_dst = root.join(format!("target/release/lib{}_v3.dylib", p.dylib_stem()));
        lipo_into(&fw_inputs, &lipo_dst)?;

        // --- Step 2: .framework bundle in tmp -------------------------------
        // Preserve `fw_build` across runs so xcodebuild's link-time
        // framework metadata cache survives; we overwrite the pieces
        // that change (dylib, plist, symlinks) idempotently below.
        let fw_dir = fw_build.join(format!("{}.framework/Versions/A", fw_name));
        fs_ctx::create_dir_all(fw_dir.join("Resources"))?;
        fs_ctx::copy(&lipo_dst, fw_dir.join(&fw_name))?;

        let status = Command::new("install_name_tool")
            .args([
                "-id",
                &format!("@rpath/{}.framework/Versions/A/{}", fw_name, fw_name),
            ])
            .arg(fw_dir.join(&fw_name))
            .status()?;
        if !status.success() {
            return Err("install_name_tool failed".into());
        }

        let fw_root = fw_build.join(format!("{}.framework", fw_name));
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
                &format!("Versions/Current/{}", fw_name),
                &fw_root.join(&fw_name),
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
        {
            let mut cs_args = vec!["--force", "--sign", sign_id];
            if is_production_identity(sign_id) {
                cs_args.extend_from_slice(&["--options", "runtime", "--timestamp"]);
            }
            cs_args.push(fw_root.to_str().unwrap());
            crate::run_codesign(&cs_args, false)?;
        }

        // --- Step 3: Xcode project scratch ---------------------------------
        // Preserve `build_dir` across runs so xcodebuild's DerivedData /
        // SYMROOT build cache survives. `write_if_changed` for every
        // source file means mtimes only bump when the embedded template
        // bytes actually shifted — xcodebuild then incrementally rebuilds
        // only the TUs that changed.
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

        let plist_path = build_dir.join("AUExt/Info.plist");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let ver = format!("{}.{}", now.as_secs(), now.subsec_millis());

        let plist = templates::au3::APPEX_INFO_PLIST
            .replace("AUVER", &ver)
            .replace("AUTYPE", p.resolved_au_type())
            .replace("AUSUB", au_v3_sub)
            .replace("AUMFR", &config.vendor.au_manufacturer)
            .replace(
                "AUNAME",
                &format!(
                    "{}: {}",
                    config.vendor.name,
                    p.au3_name.as_deref().unwrap_or(p.name.as_str()),
                ),
            )
            .replace("AUTAG", &p.au_tag);
        // AUVER regenerates every call (CFBundleVersion cache-bust for
        // hosts), so this plist's bytes shift run-to-run regardless.
        // xcodebuild still bundles the new plist but skips Swift / ObjC
        // recompilation because those sources stayed stable.
        fs_ctx::write_if_changed(&plist_path, plist)?;

        let pbx_path = build_dir.join("XcodeAUv3.xcodeproj/project.pbxproj");
        fs_ctx::write_if_changed(
            &pbx_path,
            generate_pbxproj(
                &team_id,
                &format!("{}.v3", p.bundle_id),
                &format!("{}.v3.ext", p.bundle_id),
                build_dir.join("AUExt").to_str().unwrap(),
                fw_build.to_str().unwrap(),
                &fw_name,
            ),
        )?;

        fs_ctx::write_if_changed(
            build_dir.join("App/Info.plist"),
            templates::au3::APP_INFO_PLIST,
        )?;

        // --- Step 4: xcodebuild --------------------------------------------
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
            .current_dir(&build_dir)
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

        // --- Step 5: Assemble final bundle in target/bundles ---------------
        // `ditto` preserves xattrs/ACLs/resource forks better than `cp -R`;
        // macOS code signatures survive the copy cleanly.
        let _ = fs::remove_dir_all(&final_app);
        let ditto_status = Command::new("ditto")
            .arg(&xcodebuild_app)
            .arg(&final_app)
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

        // --- Step 6: Sign inside-out against target/bundles ----------------
        // Order matters: framework first (parent bundle references its
        // signature), then appex (embeds its entitlements), then app (wraps
        // everything). Re-signing an inner bundle invalidates the outer,
        // so signing in the wrong order leaves the whole thing broken.
        let production = is_production_identity(sign_id);
        let runtime_flags: &[&str] = if production {
            &["--options", "runtime", "--timestamp"]
        } else {
            &[]
        };

        {
            let fw_path = embedded_fw.to_str().unwrap();
            let mut args = vec!["--force", "--sign", sign_id];
            args.extend_from_slice(runtime_flags);
            args.push(fw_path);
            crate::run_codesign(&args, false)?;
        }
        let entitlements_appex = build_dir.join("AUExt/AUExt.entitlements");
        let entitlements_app = build_dir.join("App/App.entitlements");
        {
            let appex_path = final_app.join("Contents/PlugIns/AUExt.appex");
            let appex_str = appex_path.to_str().unwrap();
            let ent = entitlements_appex.to_str().unwrap();
            let mut args = vec![
                "--force",
                "--sign",
                sign_id,
                "--entitlements",
                ent,
                "--generate-entitlement-der",
            ];
            args.extend_from_slice(runtime_flags);
            args.push(appex_str);
            crate::run_codesign(&args, false)?;
        }
        {
            let ent = entitlements_app.to_str().unwrap();
            let app_str = final_app.to_str().unwrap();
            let mut args = vec![
                "--force",
                "--sign",
                sign_id,
                "--entitlements",
                ent,
                "--generate-entitlement-der",
            ];
            args.extend_from_slice(runtime_flags);
            args.push(app_str);
            crate::run_codesign(&args, false)?;
        }

        crate::vprintln!("  AU v3: {}", final_app.display());
    }
    Ok(())
}

/// Install pre-built AU v3 bundles from `target/bundles/` to
/// `/Applications/` and register with pluginkit.
///
/// Expects [`emit_au_v3_bundle`] to have been called first. Batched
/// into three phases so the daemon-restart + cache-bust sequence
/// happens once per install instead of once per plugin:
///
/// 1. **Per plugin** — pre-clean stale `pluginkit` state, `sudo ditto`
///    the bundle into `/Applications/`, and `lsregister -f -R`.
/// 2. **Once for the batch** — `killall pkd` +
///    `killall AudioComponentRegistrar`, clear the AU cache, wait 2s
///    for `pkd` to respawn. Previously this ran per-plugin, wasting
///    `(N-1) × 2s` plus the daemon-respawn cost.
/// 3. **Per plugin** — `pluginkit -a` + poll-until-registered, then
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
        let final_app = root.join("target/bundles").join(format!("{app_name}.app"));
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

        // Pre-clean. `pluginkit -e ignore` only disables the registration —
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
        let _ = run_sudo("rm", &["-rf", &app_dir]);

        // Install to /Applications/. `ditto` preserves the existing
        // signature since we signed the bundle at build time.
        run_sudo("ditto", &[final_app.to_str().unwrap(), &app_dir])?;

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
    run_sudo_silent("killall", &["-9", "pkd"]);
    run_sudo_silent("killall", &["-9", "AudioComponentRegistrar"]);
    let home = dirs::home_dir().unwrap();
    let _ = fs::remove_dir_all(home.join("Library/Caches/AudioUnitCache"));
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
        crate::log_install(format!("AU3:  {}", s.app_dir));
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
/// in `target/bundles/` — mirrors the behavior of the other formats'
/// `--no-build` paths.
pub(crate) fn build_and_install_au_v3(
    root: &Path,
    config: &Config,
    plugins: &[&PluginDef],
    no_build: bool,
) -> Res {
    // Single skip gate. If we don't have a real Developer ID, don't
    // build *or* install — otherwise install_au_v3 would happily copy
    // a stale signed bundle from a previous run and produce a misleading
    // "succeeded" line in the install summary.
    let sign_id = config.macos.application_identity();
    if extract_team_id(sign_id).is_empty() {
        crate::log_skip(
            "AU v3: requires a Developer ID signing identity with a team ID. \
             Set [macos.signing].application_identity in truce.toml \
             (e.g., \"Developer ID Application: Your Name (TEAMID)\"). \
             Ad-hoc signing (\"-\") is not supported for AU v3 appex bundles."
                .to_string(),
        );
        return Ok(());
    }
    if !no_build {
        // `cargo truce install` only needs the host arch — universal
        // builds are reserved for the packaging path.
        emit_au_v3_bundle(root, config, plugins, &[MacArch::host()])?;
    }
    install_au_v3(root, config, plugins)
}

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
				PRODUCT_BUNDLE_IDENTIFIER = "com.truce.{app_id}";
				PRODUCT_NAME = "$(TARGET_NAME)";
				INFOPLIST_FILE = "App/Info.plist";
				CODE_SIGN_ENTITLEMENTS = "App/App.entitlements";
				CODE_SIGN_STYLE = Manual;
				CODE_SIGN_IDENTITY = "Developer ID Application";
				DEVELOPMENT_TEAM = {team};
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
				PRODUCT_BUNDLE_IDENTIFIER = "com.truce.{appex_id}";
				PRODUCT_NAME = "$(TARGET_NAME)";
				INFOPLIST_FILE = "AUExt/Info.plist";
				CODE_SIGN_ENTITLEMENTS = "AUExt/AUExt.entitlements";
				CODE_SIGN_STYLE = Manual;
				CODE_SIGN_IDENTITY = "Developer ID Application";
				DEVELOPMENT_TEAM = {team};
				SWIFT_VERSION = 5.0;
				MACOSX_DEPLOYMENT_TARGET = 13.0;
				APPLICATION_EXTENSION_API_ONLY = YES;
				SWIFT_OBJC_BRIDGING_HEADER = "AUExt/BridgingHeader.h";
				HEADER_SEARCH_PATHS = "{shim}";
				FRAMEWORK_SEARCH_PATHS = "{fw_search}";
				LD_RUNPATH_SEARCH_PATHS = "@executable_path/../../../../Frameworks";
				OTHER_LDFLAGS = ("-framework", "{fw}");
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
        team = team_id,
        app_id = app_bundle_id,
        appex_id = appex_bundle_id,
        shim = shim_dir,
        fw_search = fw_search,
        fw = fw_name,
    )
}
