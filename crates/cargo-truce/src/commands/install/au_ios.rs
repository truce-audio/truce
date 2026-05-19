//! AU v3 iOS pipeline.
//!
//! Builds the Rust framework dylib for the chosen iOS slice
//! (simulator or device), assembles a `.framework`, swiftc-compiles
//! the `AUExt.appex` from the `AUv3` templates, swiftc-compiles a
//! container app that discovers + instantiates + hosts the AU,
//! signs the lot, and installs onto the booted iOS Simulator or a
//! tethered device.
//!
//! Skips xcodebuild because for a one-app + one-appex + one-framework
//! bundle the direct swiftc invocations are clearer and easier to
//! iterate against than driving a pbxproj template.

#![cfg(target_os = "macos")]

use crate::util::{fs_ctx, path_str};
use crate::{PluginDef, Res};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Which iOS slice we're building: the simulator runs as native
/// `arm64` (or `x86_64` on Intel Macs); device builds target
/// `arm64` only. The two slices have distinct `LC_BUILD_VERSION`
/// platforms (`platform 7` for the simulator, `platform 2` for
/// physical iOS) and aren't lipo-able into one Mach-O - `lipo`
/// rejects them with `have the same architectures`.
#[derive(Clone, Copy)]
pub(crate) enum IosTarget {
    Simulator,
    Device,
}

impl IosTarget {
    fn rust_triple(self) -> &'static str {
        match self {
            Self::Simulator => "aarch64-apple-ios-sim",
            Self::Device => "aarch64-apple-ios",
        }
    }
    fn swift_target_suffix(self) -> &'static str {
        match self {
            Self::Simulator => "-simulator",
            Self::Device => "",
        }
    }
    fn sdk_name(self) -> &'static str {
        match self {
            Self::Simulator => "iphonesimulator",
            Self::Device => "iphoneos",
        }
    }
    fn supported_platform(self) -> &'static str {
        match self {
            Self::Simulator => "iPhoneSimulator",
            Self::Device => "iPhoneOS",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Simulator => "simulator",
            Self::Device => "device",
        }
    }
}

/// Build + install the iOS bundle for one plugin.
///
/// For the simulator target this expects `xcrun simctl boot
/// '<device>'` to have run first; for the device target the user
/// needs a paired iOS device + `ios-deploy` (or Xcode's
/// `devicectl`) on `$PATH`, plus `TRUCE_IOS_TEAM_ID` and
/// `TRUCE_IOS_PROVISIONING_PROFILE` in `.cargo/config.toml [env]`.
pub(crate) fn install_one(root: &Path, p: &PluginDef, target: IosTarget) -> Res {
    install_one_inner(root, p, target, None)
}

/// Install with an explicit `UISupportedInterfaceOrientations`
/// override for the container. The screenshot pipeline passes a
/// single-element slice so iOS forces the simulator to rotate to a
/// canonical orientation when the container launches; without that,
/// the sim inherits whatever rotation the previous test left it as,
/// and portrait-supporting
/// plug-ins can render in landscape (and vice versa), producing
/// non-deterministic baseline dimensions.
pub(crate) fn install_one_screenshot(
    root: &Path,
    p: &PluginDef,
    target: IosTarget,
    orientations_override: &[String],
) -> Res {
    install_one_inner(root, p, target, Some(orientations_override))
}

fn install_one_inner(
    root: &Path,
    p: &PluginDef,
    target: IosTarget,
    orientations_override: Option<&[String]>,
) -> Res {
    let cfg = crate::load_config()?;
    // Validate device-install prerequisites before kicking off a
    // multi-minute build pipeline. Discovering at codesign time
    // that the user hasn't set `TRUCE_IOS_PROVISIONING_PROFILE`
    // means throwing away every step before it.
    if matches!(target, IosTarget::Device) {
        if crate::ios_provisioning_profile().is_none() {
            return Err(
                "iOS device install needs TRUCE_IOS_PROVISIONING_PROFILE pointing at a \
                 .mobileprovision in .cargo/config.toml [env]."
                    .into(),
            );
        }
        // Ad-hoc signing (`-`) can never produce a device-installable
        // binary - installd requires the code-directory hash to chain
        // up to an Apple-trusted root, which only a real Developer ID
        // signing identity provides. Catching this upfront avoids a
        // confusing AMDeviceSecureInstallApplication failure later.
        let identity = crate::ios_application_identity();
        if identity == "-" || identity.trim().is_empty() {
            return Err("iOS device install needs TRUCE_IOS_SIGNING_IDENTITY (e.g. \
                 \"Apple Development: Your Name (TEAMID)\") in .cargo/config.toml [env]. \
                 Find available identities with: security find-identity -p codesigning -v"
                .into());
        }
    }
    let bundle = build_bundle(root, p, &cfg, target, orientations_override)?;
    // Full reverse-DNS identifier - the value installd records and
    // `simctl launch` / `devicectl process launch` accept. Must match
    // the CFBundleIdentifier written into the .app's Info.plist
    // (constructed the same way inside build_bundle).
    let suffix = p.bundle_id.replace('_', "-");
    let app_bundle_id = format!("{}.{suffix}", cfg.vendor.id);

    eprintln!(
        "==> Installing {} on {}...",
        bundle.display(),
        target.label()
    );
    match target {
        IosTarget::Simulator => simctl_install(&bundle, &app_bundle_id),
        IosTarget::Device => device_install(&bundle, &app_bundle_id),
    }
}

/// Build the unsigned `.app` bundle for `target`, sign it (ad-hoc
/// for simulator, identity-based for device when a profile is
/// available), and return the bundle path. Shared by the install
/// + package paths.
#[allow(clippy::too_many_lines)] // step-by-step pipeline reads top-to-bottom
pub(crate) fn build_bundle(
    root: &Path,
    p: &PluginDef,
    cfg: &crate::Config,
    target: IosTarget,
    orientations_override: Option<&[String]>,
) -> Result<PathBuf, crate::CargoTruceError> {
    let out = truce_build::target_dir(root)
        .join("ios")
        .join(target.label());
    let _ = std::fs::remove_dir_all(&out);
    fs_ctx::create_dir_all(&out)?;

    let sdk_path = run_capture("xcrun", &["--sdk", target.sdk_name(), "--show-sdk-path"])?;
    let sdk_path = sdk_path.trim();
    if sdk_path.is_empty() {
        return Err(format!(
            "xcrun could not resolve {} SDK; install Xcode CLI tools",
            target.sdk_name()
        )
        .into());
    }
    let min_ios = p.resolved_ios_minimum_os_version(&cfg.ios);
    let target_triple = format!("arm64-apple-ios{min_ios}{}", target.swift_target_suffix());

    let fw_name = format!("{}AU", capitalise_id(&p.bundle_id));
    // Full reverse-DNS CFBundleIdentifier: `{vendor.id}.{bundle_id}`.
    // `truce.toml` stores `bundle_id` as the short suffix
    // (`"synth"`); the iOS provisioning profile's wildcard App ID
    // (e.g. `TEAM.com.acme.*`) matches the assembled full ID, not
    // the bare suffix. Underscores are illegal in iOS bundle
    // identifiers; hyphens are accepted.
    let suffix = p.bundle_id.replace('_', "-");
    let app_bundle_id = format!("{}.{suffix}", cfg.vendor.id);
    let appex_bundle_id = format!("{app_bundle_id}.AUExt");
    let app_name = &p.name;
    let app_file = p.file_stem();
    let dylib_stem = p.dylib_stem();

    eprintln!(
        "==> [1/5] cargo build ({}, --features au)",
        target.rust_triple()
    );
    cargo_build_ios(&p.crate_name, target)?;
    let dylib_src = truce_build::target_dir(root)
        .join(target.rust_triple())
        .join("release")
        .join(format!("lib{dylib_stem}.dylib"));
    if !dylib_src.exists() {
        return Err(format!("missing iOS dylib: {}", dylib_src.display()).into());
    }

    eprintln!("==> [2/5] assemble {fw_name}.framework");
    let fw_dir = out.join("build").join(format!("{fw_name}.framework"));
    fs_ctx::create_dir_all(&fw_dir)?;
    let fw_bin = fw_dir.join(&fw_name);
    fs_ctx::copy(&dylib_src, &fw_bin)?;
    run(
        "install_name_tool",
        &[
            "-id",
            &format!("@rpath/{fw_name}.framework/{fw_name}"),
            path_str(&fw_bin),
        ],
    )?;
    fs_ctx::write(
        fw_dir.join("Info.plist"),
        framework_info_plist(&fw_name, &app_bundle_id, &min_ios, target),
    )?;

    let stage = out.join("build/stage");
    fs_ctx::create_dir_all(&stage)?;
    // AU v3 Swift sources come from the `include_str!`-baked
    // constants in `crate::templates::au3` rather than from disk -
    // when `cargo-truce` runs inside a downstream project (one
    // that depends on truce as a path / git dep), the templates
    // dir isn't at `<project-root>/crates/cargo-truce/templates`.
    fs_ctx::write(
        stage.join("AudioUnitFactory.swift"),
        crate::templates::au3::SWIFT_SOURCE,
    )?;
    fs_ctx::write(
        stage.join("BridgingHeader.h"),
        crate::templates::au3::BRIDGING_HEADER,
    )?;

    eprintln!("==> [3/5] AUExt.appex (swiftc)");
    let appex_dir = out.join("build/AUExt.appex");
    fs_ctx::create_dir_all(appex_dir.join("Frameworks"))?;

    let au_type = p.resolved_au_type();
    let au_sub = p.resolved_fourcc();
    let au_mfr = &cfg.vendor.au_manufacturer;
    let au_tag = &p.au_tag;

    let appex_info =
        crate::templates::au3::render_appex_info_plist(&crate::templates::au3::AppexPlistValues {
            au_name: app_name,
            au_type,
            au_sub,
            au_mfr,
            au_tag,
            au_ver: "1",
            min_os: &min_ios,
            supported_platform: target.supported_platform(),
            xcode_tokens: Some(crate::templates::au3::XcodeTokens {
                executable_name: "AUExt",
                bundle_id: &appex_bundle_id,
                package_type: "XPC!",
                module_name: "AUExt",
            }),
        });
    fs_ctx::write(appex_dir.join("Info.plist"), appex_info)?;

    // Write `au_shim_types.h` from the `include_str!`-baked
    // constant (re-exported through `truce-shim-types`) into the
    // stage dir so the bridging header's `#import` resolves
    // without depending on the truce checkout layout - downstream
    // projects that depend on truce as a path / git dep don't
    // have a `crates/truce-shim-types/include` under their root.
    fs_ctx::write(
        stage.join("au_shim_types.h"),
        crate::templates::au3::SHIM_TYPES_H,
    )?;
    let shim_include = stage.clone();
    let appex_bin = appex_dir.join("AUExt");

    let appex_status = Command::new("xcrun")
        .args(["-sdk", target.sdk_name(), "swiftc"])
        .args([
            "-target",
            &target_triple,
            "-sdk",
            sdk_path,
            "-F",
            path_str(&out.join("build")),
            "-framework",
            &fw_name,
            "-framework",
            "Foundation",
            "-framework",
            "UIKit",
            "-framework",
            "AVFAudio",
            "-framework",
            "AudioToolbox",
            "-framework",
            "CoreAudioKit",
            "-module-name",
            "AUExt",
            "-emit-executable",
            "-import-objc-header",
            path_str(&stage.join("BridgingHeader.h")),
            "-Xcc",
            &format!("-I{}", shim_include.display()),
            "-Xcc",
            "-isysroot",
            "-Xcc",
            sdk_path,
            "-Xcc",
            "-target",
            "-Xcc",
            &target_triple,
            "-O",
            "-Xlinker",
            "-ObjC",
            // App-Extension principal-class entry point - swiftc's
            // default `main` is a no-op stub that exits immediately.
            "-Xlinker",
            "-e",
            "-Xlinker",
            "_NSExtensionMain",
            "-Xlinker",
            "-rpath",
            "-Xlinker",
            "@executable_path/Frameworks",
            "-Xlinker",
            "-rpath",
            "-Xlinker",
            "@loader_path/../../Frameworks",
            "-o",
            path_str(&appex_bin),
            path_str(&stage.join("AudioUnitFactory.swift")),
        ])
        .status()
        .map_err(|e| format!("swiftc (appex): {e}"))?;
    if !appex_status.success() {
        return Err(format!("swiftc appex exited {appex_status}").into());
    }
    crate::copy_dir_recursive(
        &fw_dir,
        &appex_dir
            .join("Frameworks")
            .join(format!("{fw_name}.framework")),
    )?;

    eprintln!("==> [4/5] {app_file}.app (swiftc)");
    let app_dir = out.join(format!("{app_file}.app"));
    fs_ctx::create_dir_all(app_dir.join("PlugIns"))?;
    fs_ctx::create_dir_all(app_dir.join("Frameworks"))?;

    let app_main_src = stage.join("AppMain.swift");
    // User-facing description for the container's "About this
    // plugin" section. Plugin authors can override via
    // `description = "..."` in their `[[plugin]]` entry; the
    // category-aware fallback satisfies App Store review's
    // "the app must explain what it is" heuristic without forcing
    // every plugin to carry a description string.
    let description = p
        .description
        .clone()
        .unwrap_or_else(|| default_description(p));
    // Per-plugin URL override takes precedence over the
    // vendor-level URL. Useful in suites where individual plug-ins
    // ship with their own product pages.
    let vendor_url = p
        .ios_url
        .as_deref()
        .or(cfg.vendor.url.as_deref())
        .unwrap_or("https://truce.audio/")
        .to_string();
    fs_ctx::write(
        &app_main_src,
        render_app_main_swift(
            app_name,
            &cfg.vendor.name,
            &description,
            &vendor_url,
            p.ios_scale_editor_to_fit,
            p.mute_preview_output,
        ),
    )?;

    let app_status = Command::new("xcrun")
        .args(["-sdk", target.sdk_name(), "swiftc"])
        .args([
            "-target",
            &target_triple,
            "-sdk",
            sdk_path,
            "-F",
            path_str(&out.join("build")),
            "-framework",
            &fw_name,
            "-framework",
            "UIKit",
            "-framework",
            "Foundation",
            "-framework",
            "AVFAudio",
            "-framework",
            "AudioToolbox",
            "-framework",
            "CoreAudioKit",
            "-module-name",
            "App",
            "-emit-executable",
            "-parse-as-library",
            "-import-objc-header",
            path_str(&stage.join("BridgingHeader.h")),
            "-Xcc",
            &format!("-I{}", shim_include.display()),
            "-Xcc",
            "-isysroot",
            "-Xcc",
            sdk_path,
            "-Xcc",
            "-target",
            "-Xcc",
            &target_triple,
            "-O",
            "-Xlinker",
            "-rpath",
            "-Xlinker",
            "@executable_path/Frameworks",
            "-o",
            path_str(&app_dir.join("App")),
            path_str(&app_main_src),
        ])
        .status()
        .map_err(|e| format!("swiftc (app): {e}"))?;
    if !app_status.success() {
        return Err(format!("swiftc app exited {app_status}").into());
    }
    let orientation_tokens: Vec<String> = orientations_override
        .map(<[String]>::to_vec)
        .or_else(|| p.ios_orientations.clone())
        .unwrap_or_else(|| {
            DEFAULT_IOS_ORIENTATIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        });
    let orientations_xml = render_orientation_array(&orientation_tokens)?;
    fs_ctx::write(
        app_dir.join("Info.plist"),
        app_info_plist(
            app_name,
            &app_bundle_id,
            &min_ios,
            target,
            &orientations_xml,
        ),
    )?;

    // Optional icon set. Two paths:
    //   1. The vendor supplies a real Xcode `.appiconset` directory.
    //      We hand it to `actool` (Xcode asset compiler) which emits
    //      an `Assets.car` + the `CFBundleIcons` plist additions for
    //      every required @1x / @2x / @3x slot. That artifact is
    //      what the App Store + iOS home screen scanner expect.
    //   2. `actool` is missing or the path doesn't look like an
    //      appiconset → fall back to copying the source PNGs into
    //      the bundle root + injecting a minimal `CFBundleIconFiles`
    //      array in the Info.plist. Works on the simulator + ad-hoc
    //      path; App Store ingestion will reject.
    let icon_plist_additions = embed_app_icon(p, root, &app_dir, target, &min_ios)?;
    if !icon_plist_additions.is_empty() {
        // Splice the icon-related plist keys into the
        // already-written `Info.plist`. Simpler than re-running
        // `app_info_plist` with the additions because the
        // helper's `format!` template doesn't carry the keys
        // unconditionally - most plugins don't supply an icon.
        let info_path = app_dir.join("Info.plist");
        let raw = std::fs::read_to_string(&info_path)
            .map_err(|e| format!("read {}: {e}", info_path.display()))?;
        let patched = raw.replacen("</dict>", &format!("{icon_plist_additions}</dict>"), 1);
        fs_ctx::write(&info_path, patched)?;
    }

    eprintln!("==> [5/5] assemble + codesign {app_file}.app");
    crate::copy_dir_recursive(&appex_dir, &app_dir.join("PlugIns/AUExt.appex"))?;
    crate::copy_dir_recursive(
        &fw_dir,
        &app_dir
            .join("Frameworks")
            .join(format!("{fw_name}.framework")),
    )?;

    // Entitlements: written into the bundle stage dir, passed to
    // codesign via `--entitlements`. App Group (when configured)
    // shows up on both the container app and the appex so
    // `fullState` blobs + preset files round-trip across the
    // sandbox boundary via the shared container.
    let app_ent = stage.join("App.entitlements");
    let appex_ent = stage.join("AUExt.entitlements");
    // Device installs need `application-identifier` (TEAMID.bundle.id)
    // in the entitlements; iOS installd matches that against the
    // embedded mobileprovision's allow-list. Pull the team ID from
    // the env var (set by the user) OR extract it from the profile
    // itself - typical workflow has only the profile path set.
    let team_id: Option<String> = if matches!(target, IosTarget::Device) {
        crate::ios_team_id().or_else(|| {
            crate::ios_provisioning_profile()
                .as_deref()
                .and_then(extract_team_id_from_profile)
        })
    } else {
        None
    };
    let team_for_app = team_id.as_deref();
    fs_ctx::write(
        &app_ent,
        render_entitlements_plist(p.resolved_ios_app_group(), &app_bundle_id, team_for_app),
    )?;
    fs_ctx::write(
        &appex_ent,
        render_entitlements_plist(p.resolved_ios_app_group(), &appex_bundle_id, team_for_app),
    )?;

    let identity = signing_identity_for(target);
    let appex_prof_env = crate::ios_appex_provisioning_profile();
    let mobileprovision = if matches!(target, IosTarget::Device) {
        let app_prof =
            crate::ios_provisioning_profile().ok_or_else(|| -> crate::CargoTruceError {
                "iOS device install needs TRUCE_IOS_PROVISIONING_PROFILE pointing at a \
             .mobileprovision in .cargo/config.toml [env]"
                    .into()
            })?;
        // iOS installd validates BOTH the container app and the
        // appex against their own embedded.mobileprovision. A
        // mismatched / missing appex profile produces error
        // 0xe8008015 ("A valid provisioning profile for this
        // executable was not found") at AMDeviceSecureInstall
        // time. Two paths:
        //   1. The container's profile is a wildcard
        //      (`<prefix>.*`) that covers both bundle IDs - reuse
        //      it for the appex too.
        //   2. TRUCE_IOS_APPEX_PROVISIONING_PROFILE is set to a
        //      separate profile bound to the `<bundle>.AUExt` ID.
        let appex_prof = appex_prof_env.as_ref().unwrap_or(&app_prof);
        fs_ctx::copy(&app_prof, app_dir.join("embedded.mobileprovision"))?;
        fs_ctx::copy(
            appex_prof,
            app_dir.join("PlugIns/AUExt.appex/embedded.mobileprovision"),
        )?;
        Some(app_prof)
    } else {
        None
    };

    codesign(
        &app_dir
            .join("Frameworks")
            .join(format!("{fw_name}.framework")),
        &identity,
        None,
    )?;
    codesign(
        &app_dir.join("PlugIns/AUExt.appex"),
        &identity,
        Some(&appex_ent),
    )?;
    codesign(&app_dir, &identity, Some(&app_ent))?;

    let _ = mobileprovision; // kept-alive for the duration of codesign

    Ok(app_dir)
}

fn simctl_install(bundle: &Path, app_bundle_id: &str) -> Res {
    let installed = Command::new("xcrun")
        .args(["simctl", "install", "booted"])
        .arg(bundle)
        .status();
    match installed {
        Ok(s) if s.success() => {
            eprintln!(
                "Installed: {}\nLaunch with: xcrun simctl launch booted {app_bundle_id}",
                bundle.display()
            );
            Ok(())
        }
        Ok(s) => Err(format!(
            "simctl install exited {s}; boot a simulator first (xcrun simctl boot '<device>')"
        )
        .into()),
        Err(e) => Err(format!("xcrun simctl install booted: {e}").into()),
    }
}

fn device_install(bundle: &Path, app_bundle_id: &str) -> Res {
    // Prefer ios-deploy when present (mature, no Xcode dependency for
    // install). Fall back to `xcrun devicectl device install app …`
    // which ships with Xcode 15+. Both expect a paired + trusted
    // device.
    let use_ios_deploy = Command::new("which")
        .arg("ios-deploy")
        .status()
        .is_ok_and(|s| s.success());
    let status = if use_ios_deploy {
        Command::new("ios-deploy")
            .args(["--bundle"])
            .arg(bundle)
            .arg("--no-wifi")
            .status()
    } else {
        Command::new("xcrun")
            .args([
                "devicectl",
                "device",
                "install",
                "app",
                "--device",
                ".",
                "--",
            ])
            .arg(bundle)
            .status()
    };
    match status {
        Ok(s) if s.success() => {
            eprintln!(
                "Installed on device: {}\nLaunch from the home screen, or:\n  \
                 xcrun devicectl device process launch --device . {app_bundle_id}",
                bundle.display()
            );
            Ok(())
        }
        Ok(s) => Err(format!(
            "device install exited {s}; pair + trust an iOS device first \
             (xcrun devicectl list devices)"
        )
        .into()),
        Err(e) => Err(format!("device install: {e}").into()),
    }
}

// ---------------------------------------------------------------------------
// xcframework helper
// ---------------------------------------------------------------------------

/// Build a two-slice `.xcframework` containing the iOS device +
/// simulator `.framework` slices for the plugin. Output lands at
/// `<target>/ios/xcframework/<fw_name>.xcframework`. Consumed by
/// the `package --ios` path; install paths pick a single slice and
/// skip this step.
pub(crate) fn build_xcframework(
    root: &Path,
    p: &PluginDef,
) -> Result<PathBuf, crate::CargoTruceError> {
    let cfg = crate::load_config()?;
    let out = truce_build::target_dir(root).join("ios/xcframework");
    let _ = std::fs::remove_dir_all(&out);
    fs_ctx::create_dir_all(&out)?;
    let fw_name = format!("{}AU", capitalise_id(&p.bundle_id));
    let min_ios = p.resolved_ios_minimum_os_version(&cfg.ios);
    // Same `{vendor.id}.{suffix}` construction as build_bundle.
    let suffix = p.bundle_id.replace('_', "-");
    let app_bundle_id = format!("{}.{suffix}", cfg.vendor.id);
    let mut slices: Vec<PathBuf> = Vec::with_capacity(2);
    for target in [IosTarget::Device, IosTarget::Simulator] {
        eprintln!(
            "==> xcframework slice: cargo build ({})",
            target.rust_triple()
        );
        cargo_build_ios(&p.crate_name, target)?;
        let dylib_src = truce_build::target_dir(root)
            .join(target.rust_triple())
            .join("release")
            .join(format!("lib{}.dylib", p.dylib_stem()));
        if !dylib_src.exists() {
            return Err(format!("missing iOS dylib: {}", dylib_src.display()).into());
        }
        // Each slice lives in its own per-target subdirectory so
        // the `.framework` keeps its real name (`GainAU.framework`,
        // not `device-GainAU.framework`). xcodebuild's
        // `-create-xcframework` matches the binary inside the
        // framework against the framework's directory stem; renaming
        // the directory would force the binary to be renamed too,
        // breaking `install_name_tool -id @rpath/<fw>.framework/<fw>`.
        let slice_root = out.join(target.label());
        fs_ctx::create_dir_all(&slice_root)?;
        let slice_dir = slice_root.join(format!("{fw_name}.framework"));
        fs_ctx::create_dir_all(&slice_dir)?;
        let fw_bin = slice_dir.join(&fw_name);
        fs_ctx::copy(&dylib_src, &fw_bin)?;
        run(
            "install_name_tool",
            &[
                "-id",
                &format!("@rpath/{fw_name}.framework/{fw_name}"),
                path_str(&fw_bin),
            ],
        )?;
        fs_ctx::write(
            slice_dir.join("Info.plist"),
            framework_info_plist(&fw_name, &app_bundle_id, &min_ios, target),
        )?;
        slices.push(slice_dir);
    }
    let xcfw_out = out.join(format!("{fw_name}.xcframework"));
    // xcodebuild refuses to write into an existing output dir -
    // a stale leftover from a previous failed run trips the
    // "couldn't be copied because an item with the same name
    // already exists" message. Clean the slot first.
    let _ = std::fs::remove_dir_all(&xcfw_out);
    let mut cmd = Command::new("xcodebuild");
    cmd.arg("-create-xcframework");
    for slice in &slices {
        cmd.arg("-framework").arg(slice);
    }
    cmd.arg("-output").arg(&xcfw_out);
    let status = cmd
        .status()
        .map_err(|e| format!("xcodebuild -create-xcframework: {e}"))?;
    if !status.success() {
        return Err(format!("xcodebuild -create-xcframework exited {status}").into());
    }
    Ok(xcfw_out)
}

// ---------------------------------------------------------------------------
// .ipa packaging
// ---------------------------------------------------------------------------

/// Build a device-targeted signed `.app`, wrap it into a `.ipa`
/// (`Payload/{App}.app/...`). The signing identity comes from
/// `TRUCE_IOS_SIGNING_IDENTITY` (typically `"Apple Distribution: …"`
/// for App Store / `TestFlight` submissions). Notarisation /
/// `altool` upload is intentionally out of scope - that's a
/// distribution step, not a build step.
pub(crate) fn package_ipa(root: &Path, p: &PluginDef) -> Result<PathBuf, crate::CargoTruceError> {
    let cfg = crate::load_config()?;
    let app_dir = build_bundle(root, p, &cfg, IosTarget::Device, None)?;
    let out_dir = truce_build::target_dir(root).join("ios/ipa");
    let _ = std::fs::remove_dir_all(&out_dir);
    fs_ctx::create_dir_all(&out_dir)?;
    let payload = out_dir.join("Payload");
    fs_ctx::create_dir_all(&payload)?;
    let file_name = app_dir
        .file_name()
        .ok_or_else(|| -> crate::CargoTruceError {
            format!("app bundle has no file name: {}", app_dir.display()).into()
        })?;
    crate::copy_dir_recursive(&app_dir, &payload.join(file_name))?;
    let ipa_path = out_dir.join(format!("{}.ipa", p.file_stem()));
    // `zip -r` over `Payload/` is the canonical Apple shape - the
    // `.ipa` extension is documentation. Strip resource forks
    // (`-X`) so Linux / Windows hosts that unpack the ipa don't see
    // AppleDouble metadata files.
    let status = Command::new("zip")
        .current_dir(&out_dir)
        .args(["-r", "-X", "-q"])
        .arg(&ipa_path)
        .arg("Payload")
        .status()
        .map_err(|e| format!("zip: {e}"))?;
    if !status.success() {
        return Err(format!("zip -r {} Payload/ exited {status}", ipa_path.display()).into());
    }
    Ok(ipa_path)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Embed the plugin's app icon into the bundle. Returns the
/// additional `Info.plist` keys that need to be spliced into the
/// existing `<dict>` block, or an empty string when no icon was
/// configured.
///
/// Compilation strategy:
///
/// - When `xcrun actool` is on `$PATH` *and* the source looks like
///   an `.appiconset` (i.e. a directory containing
///   `Contents.json`), drive `actool --compile` and let it emit
///   `Assets.car` + the plist additions. App Store / iOS scanner-
///   compatible.
/// - Otherwise, copy raw `.png`s into the bundle root and emit a
///   minimal `CFBundleIconFiles` array. Works for simulator +
///   ad-hoc smoke testing; App Store ingestion will reject.
fn embed_app_icon(
    p: &PluginDef,
    root: &Path,
    app_dir: &Path,
    target: IosTarget,
    min_ios: &str,
) -> Result<String, crate::CargoTruceError> {
    let Some(path) = p.ios_icon_set.as_deref() else {
        return Ok(String::new());
    };
    let src = root.join(path);
    if !src.exists() {
        eprintln!("warning: ios_icon_set={path} does not resolve - skipping icon embed");
        return Ok(String::new());
    }
    let is_appiconset = src.join("Contents.json").exists();
    let actool_available = Command::new("xcrun")
        .args(["--find", "actool"])
        .output()
        .is_ok_and(|o| o.status.success());
    if is_appiconset && actool_available {
        // actool's `--app-icon <NAME>` argument is the .appiconset's
        // stem (directory name minus the `.appiconset` extension).
        // actool itself wants a parent `.xcassets` catalog, NOT a
        // bare `.appiconset` - it scans the catalog looking for an
        // app-icon set whose name matches `--app-icon`. Construct
        // a one-shot catalog wrapping the user's iconset so the
        // user doesn't have to maintain an .xcassets dir themselves.
        let icon_name = src
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("AppIcon")
            .to_string();
        let catalog = app_dir
            .parent()
            .unwrap_or(app_dir)
            .join("_truce-icon-catalog.xcassets");
        let _ = std::fs::remove_dir_all(&catalog);
        fs_ctx::create_dir_all(&catalog)?;
        crate::copy_dir_recursive(&src, &catalog.join(src.file_name().unwrap()))?;
        // The `.xcassets` itself wants a minimal Contents.json or
        // actool warns; provide the boilerplate.
        fs_ctx::write(
            catalog.join("Contents.json"),
            "{\n  \"info\" : { \"author\" : \"xcode\", \"version\" : 1 }\n}\n",
        )?;
        let info_out = app_dir.join("actool-partial.plist");
        let status = Command::new("xcrun")
            .args([
                "actool",
                "--app-icon",
                &icon_name,
                "--minimum-deployment-target",
                min_ios,
                "--platform",
                // Asset catalogs are platform-specific; build for the
                // slice we're shipping. The iphoneos catalog differs
                // from iphonesimulator in its Asset.car device/scale
                // entries; mixing them up trips installd's catalog
                // validation on the wrong target.
                target.sdk_name(),
                "--target-device",
                "iphone",
                "--target-device",
                "ipad",
                "--output-partial-info-plist",
            ])
            .arg(&info_out)
            .args(["--compile"])
            .arg(app_dir)
            .arg(&catalog)
            .status()
            .map_err(|e| format!("xcrun actool: {e}"))?;
        let _ = std::fs::remove_dir_all(&catalog);
        if status.success() {
            let additions = std::fs::read_to_string(&info_out)
                .map_err(|e| format!("read {}: {e}", info_out.display()))?;
            let _ = std::fs::remove_file(&info_out);
            // The partial plist actool emits is a full `<plist>`
            // wrapper; extract the inner `<dict>` body so we can
            // splice it into the existing Info.plist.
            return Ok(extract_plist_body(&additions));
        }
        eprintln!(
            "warning: actool exited {status} compiling {} - falling back to raw PNG copy",
            src.display()
        );
    }
    // Fallback: copy every `.png` and emit a CFBundleIconFiles list
    // referencing each by stem. iOS picks the closest @1x/@2x/@3x
    // match by filename suffix at runtime.
    let mut icon_files: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&src).map_err(|e| format!("read_dir {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| format!("dirent {}: {e}", src.display()))?;
        if entry.path().extension().and_then(std::ffi::OsStr::to_str) == Some("png") {
            fs_ctx::copy(entry.path(), app_dir.join(entry.file_name()))?;
            if let Some(stem) = entry.path().file_stem().and_then(std::ffi::OsStr::to_str) {
                icon_files.push(stem.to_string());
            }
        }
    }
    if icon_files.is_empty() {
        return Ok(String::new());
    }
    let mut s = String::from("    <key>CFBundleIconFiles</key>\n    <array>\n");
    for name in &icon_files {
        let _ = writeln!(s, "        <string>{name}</string>");
    }
    s.push_str("    </array>\n");
    Ok(s)
}

/// Pull the `<dict>` body out of a `<plist>...</plist>` document
/// `actool` emits as `--output-partial-info-plist`. We only want
/// the inner key/value pairs for splicing.
fn extract_plist_body(plist: &str) -> String {
    let Some(start) = plist.find("<dict>") else {
        return String::new();
    };
    let Some(end) = plist.rfind("</dict>") else {
        return String::new();
    };
    let inner = &plist[start + "<dict>".len()..end];
    // Re-indent so the splice fits the existing Info.plist's
    // 4-space body.
    let mut out = String::new();
    for line in inner.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push_str("    ");
        out.push_str(trimmed);
        out.push('\n');
    }
    out
}

/// Pull the team identifier out of a `.mobileprovision`. The file
/// is CMS-signed; `security cms -D -i <profile>` decodes it to
/// plist XML containing a `TeamIdentifier` array - first entry is
/// the active team ID. Returns `None` if the decode fails or the
/// key isn't present (very unusual for a real Apple-issued profile).
fn extract_team_id_from_profile(profile: &Path) -> Option<String> {
    let out = Command::new("security")
        .args(["cms", "-D", "-i"])
        .arg(profile)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let xml = String::from_utf8_lossy(&out.stdout);
    // Hand-rolled extraction. The plist's structure is fixed:
    //   <key>TeamIdentifier</key>
    //   <array>
    //       <string>ABCD1234EF</string>
    //   </array>
    let key_pos = xml.find("<key>TeamIdentifier</key>")?;
    let after = &xml[key_pos..];
    let s_start = after.find("<string>")? + "<string>".len();
    let s_end = after[s_start..].find("</string>")?;
    let team = &after[s_start..s_start + s_end];
    if team.is_empty() {
        None
    } else {
        Some(team.to_string())
    }
}

fn cargo_build_ios(crate_name: &str, target: IosTarget) -> Res {
    let status = Command::new("cargo")
        .args([
            "build",
            "-p",
            crate_name,
            "--target",
            target.rust_triple(),
            "--release",
            "--no-default-features",
            "--features",
            "au",
        ])
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build exited {status}").into());
    }
    Ok(())
}

fn signing_identity_for(target: IosTarget) -> String {
    match target {
        // Simulator runs all binaries ad-hoc-signed unless the user
        // pins an iOS-specific identity. macOS `application_identity`
        // is the wrong default for iOS but acceptable for sim
        // smoke testing.
        IosTarget::Simulator => "-".to_string(),
        IosTarget::Device => crate::ios_application_identity(),
    }
}

fn codesign(path: &Path, identity: &str, entitlements: Option<&Path>) -> Res {
    let mut cmd = Command::new("codesign");
    cmd.args(["--force", "--sign", identity, "--timestamp=none"]);
    if let Some(ent) = entitlements {
        cmd.arg("--entitlements").arg(ent);
    }
    cmd.arg(path);
    let status = cmd.status().map_err(|e| format!("codesign: {e}"))?;
    if !status.success() {
        return Err(format!("codesign exited {status} for {}", path.display()).into());
    }
    Ok(())
}

fn run(cmd: &str, args: &[&str]) -> Res {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if !status.success() {
        return Err(format!("{cmd} exited {status}").into());
    }
    Ok(())
}

fn run_capture(cmd: &str, args: &[&str]) -> Result<String, crate::CargoTruceError> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd} exited {}", out.status).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `"fundsp-reverb-simple"` → `"FundspReverbSimple"`
fn capitalise_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cap = true;
    for c in s.chars() {
        if c == '_' || c == '-' {
            cap = true;
            continue;
        }
        if cap {
            out.extend(c.to_uppercase());
            cap = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn framework_info_plist(
    fw_name: &str,
    bundle_id: &str,
    min_ios: &str,
    target: IosTarget,
) -> String {
    let platform = target.supported_platform();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleExecutable</key><string>{fw_name}</string>
    <key>CFBundleIdentifier</key><string>{bundle_id}.framework</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>{fw_name}</string>
    <key>CFBundlePackageType</key><string>FMWK</string>
    <key>CFBundleShortVersionString</key><string>1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>MinimumOSVersion</key><string>{min_ios}</string>
    <key>CFBundleSupportedPlatforms</key><array><string>{platform}</string></array>
</dict>
</plist>
"#
    )
}

/// Default orientation set when a plug-in doesn't declare its own.
/// Matches the historical behaviour: portrait + both landscapes,
/// no portrait-upside-down (audio apps don't generally use it).
const DEFAULT_IOS_ORIENTATIONS: &[&str] = &["portrait", "landscape-left", "landscape-right"];

/// Convert the TOML-friendly orientation token into the
/// `UIInterfaceOrientation*` constant iOS expects in the
/// `UISupportedInterfaceOrientations` plist array.
fn map_orientation(token: &str) -> Result<&'static str, crate::CargoTruceError> {
    Ok(match token {
        "portrait" => "UIInterfaceOrientationPortrait",
        "portrait-upside-down" => "UIInterfaceOrientationPortraitUpsideDown",
        "landscape-left" => "UIInterfaceOrientationLandscapeLeft",
        "landscape-right" => "UIInterfaceOrientationLandscapeRight",
        other => {
            return Err(format!(
                "ios_orientations: unknown value `{other}`; expected one of \
                 portrait / portrait-upside-down / landscape-left / landscape-right"
            )
            .into());
        }
    })
}

/// Build the `<string>…</string>` lines for the
/// `UISupportedInterfaceOrientations` array. Returns the joined
/// inner XML (no `<array>` wrapper). Rejects empty input and
/// unknown tokens - empty would let iOS reject the bundle at
/// install time with a less actionable message.
fn render_orientation_array(tokens: &[String]) -> Result<String, crate::CargoTruceError> {
    if tokens.is_empty() {
        return Err("ios_orientations: list must contain at least one entry".into());
    }
    let mut out = String::new();
    for t in tokens {
        out.push_str("        <string>");
        out.push_str(map_orientation(t)?);
        out.push_str("</string>\n");
    }
    Ok(out)
}

fn app_info_plist(
    app_name: &str,
    bundle_id: &str,
    min_ios: &str,
    target: IosTarget,
    orientations_xml: &str,
) -> String {
    let platform = target.supported_platform();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleExecutable</key><string>App</string>
    <key>CFBundleIdentifier</key><string>{bundle_id}</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>{app_name}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSRequiresIPhoneOS</key><true/>
    <key>MinimumOSVersion</key><string>{min_ios}</string>
    <key>CFBundleSupportedPlatforms</key><array><string>{platform}</string></array>
    <key>NSMicrophoneUsageDescription</key><string>{app_name} can route your device's microphone through the plug-in so you can hear it process live audio - useful for previewing effects without a DAW.</string>
    <key>NSBluetoothAlwaysUsageDescription</key><string>{app_name} discovers Bluetooth MIDI controllers paired in Settings so you can play / control the plug-in from an external keyboard.</string>
    <key>UILaunchScreen</key><dict/>
    <key>UISupportedInterfaceOrientations</key>
    <array>
{orientations_xml}    </array>
</dict>
</plist>
"#
    )
}

/// Build the `<plist>` content for an `.entitlements` file.
///
/// For device installs (`team_id_for_app_id.is_some()`), iOS
/// validates `application-identifier` + `com.apple.developer.team-identifier`
/// against the embedded `.mobileprovision`. Without them the
/// signed binary's claim falls back to the implicit Info.plist
/// bundle ID with no team prefix - installd rejects with error
/// 0xe8008015 ("A valid provisioning profile for this executable
/// was not found"). For ad-hoc simulator installs the entitlement
/// is omitted (no profile means nothing to validate against).
fn render_entitlements_plist(
    app_group: Option<&str>,
    bundle_id: &str,
    team_id_for_app_id: Option<&str>,
) -> String {
    let mut keys = String::new();
    // `com.apple.security.app-sandbox` and `network.client` are
    // macOS-only entitlements; iOS apps don't carry them because the
    // platform sandboxes everything by default. Claiming them in an
    // iOS-signed binary triggers a profile-validation failure at
    // `AMDeviceSecureInstallApplication` (error 0xe8008015): the
    // iOS profile doesn't list those keys in its Entitlements
    // allow-list, so the binary's signed claim is rejected as "not
    // granted by the profile".
    if let Some(group) = app_group {
        keys.push_str("    <key>com.apple.security.application-groups</key>\n    <array>\n");
        let _ = writeln!(keys, "        <string>{group}</string>");
        keys.push_str("    </array>\n");
    }
    if let Some(team) = team_id_for_app_id {
        // Development builds need `get-task-allow` so lldb / `os.log`
        // public-string capture / Instruments can attach. Production
        // (Apple Distribution) builds must not carry it; a release
        // path that signs with a Distribution identity needs to gate
        // this on the identity kind before shipping.
        let _ = writeln!(keys, "    <key>application-identifier</key>");
        let _ = writeln!(keys, "    <string>{team}.{bundle_id}</string>");
        let _ = writeln!(keys, "    <key>com.apple.developer.team-identifier</key>");
        let _ = writeln!(keys, "    <string>{team}</string>");
        let _ = writeln!(keys, "    <key>get-task-allow</key>");
        let _ = writeln!(keys, "    <true/>");
        let _ = writeln!(keys, "    <key>keychain-access-groups</key>");
        let _ = writeln!(keys, "    <array>");
        let _ = writeln!(keys, "        <string>{team}.{bundle_id}</string>");
        let _ = writeln!(keys, "    </array>");
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
{keys}</dict>
</plist>
"#
    )
}

/// Category-aware fallback shown when truce.toml omits an explicit
/// `description`. Keeps the About pane non-empty so App Store review
/// doesn't flag the app as a stub.
fn default_description(p: &PluginDef) -> String {
    let kind = match p.category.as_str() {
        "instrument" => "instrument",
        "midi" | "note_effect" => "MIDI processor",
        "analyzer" => "audio analyzer",
        _ => "audio effect",
    };
    format!(
        "{} is an {kind} for AUv3-compatible hosts like GarageBand, \
         AUM, Cubasis, and Logic Pro for iPad.",
        p.name
    )
}

/// Escape a string for safe embedding in a Swift single-line string
/// literal. `\` → `\\`, `"` → `\"`, real newlines → `\n` (so a
/// multi-line TOML `description = """..."""` value flattens into one
/// Swift line; the Swift label renders the `\n` as a line break at
/// runtime). Tab + CR similarly normalised.
fn swift_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn render_app_main_swift(
    app_name: &str,
    vendor_name: &str,
    description: &str,
    vendor_url: &str,
    scale_editor_to_fit: bool,
    mute_preview_output: bool,
) -> String {
    // Source lives in `templates/au_ios/AppMain.swift` (compiled
    // in via `include_str!`); we substitute the placeholder tokens
    // the template carries. Everything else (Swift braces, string
    // interpolation `\(expr)`, etc.) is left intact.
    let description = swift_escape(description);
    let vendor_name = swift_escape(vendor_name);
    let vendor_url = swift_escape(vendor_url);
    let bool_token = |b: bool| if b { "true" } else { "false" };
    crate::templates::au_ios::APP_MAIN
        .replace("{app_name}", app_name)
        .replace("{vendor_name}", &vendor_name)
        .replace("{description}", &description)
        .replace("{vendor_url}", &vendor_url)
        .replace("{ios_scale_editor_to_fit}", bool_token(scale_editor_to_fit))
        .replace("{mute_preview_output}", bool_token(mute_preview_output))
}
