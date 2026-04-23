//! AU v3 appex install: build the Rust framework, generate an Xcode project,
//! drive xcodebuild, sign, and register the appex with pluginkit.

use crate::templates;
use crate::util::fs_ctx;
use crate::{
    cargo_build_for_arch, deployment_target, dirs, extract_team_id, is_production_identity,
    lipo_into, release_lib_for_target, run_sudo, tmp_dir, Config, MacArch, PluginDef, Res,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn build_au_v3(
    root: &Path,
    config: &Config,
    plugins: &[&PluginDef],
    no_build: bool,
    archs: &[MacArch],
) -> Res {
    let sign_id = config.macos.application_identity();
    let team_id = extract_team_id(sign_id);
    let dt = &deployment_target();

    if team_id.is_empty() {
        eprintln!("AU v3: skipping — requires a Developer ID signing identity with a team ID.");
        eprintln!("  Set [macos.signing].application_identity in truce.toml to your Developer ID certificate,");
        eprintln!("  e.g., \"Developer ID Application: Your Name (TEAMID)\"");
        eprintln!("  Ad-hoc signing (\"-\") is not supported for AU v3 appex bundles.");
        return Ok(());
    }

    if archs.is_empty() {
        return Err("build_au_v3: empty archs list".into());
    }

    for p in plugins {
        let fw_name = p.fw_name();
        let au_v3_sub = p.au3_sub();
        let app_dir = format!("/Applications/{}.app", p.au3_app_name());
        let appex_id = format!(
            "com.{}.{}.v3.ext",
            config.vendor.id.trim_start_matches("com."),
            p.suffix
        );
        let build_dir = tmp_dir().join(format!("au_v3_build_{}", p.suffix));
        let fw_build = tmp_dir().join(format!("au_v3_fw_{}", p.suffix));

        eprintln!("Building AU v3 ({})...", p.name);

        if !no_build {
            // Step 1: Build Rust framework, once per arch, then lipo into the
            // canonical `lib{stem}_v3.dylib` location.
            for &arch in archs {
                eprintln!("  Building Rust framework ({})...", arch.triple());
                let mut env_pairs: Vec<(&str, &str)> = vec![
                    ("TRUCE_AU_VERSION", "3"),
                    ("TRUCE_AU_PLUGIN_ID", &p.suffix),
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
                let src = release_lib_for_target(
                    root,
                    &p.dylib_stem(),
                    Some(arch.triple()),
                );
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
                    release_lib_for_target(
                        root,
                        &format!("{}_v3", p.dylib_stem()),
                        Some(a.triple()),
                    )
                })
                .collect();
            let dst = root.join(format!(
                "target/release/lib{}_v3.dylib",
                p.dylib_stem()
            ));
            lipo_into(&fw_inputs, &dst)?;

            // Step 2: Create .framework bundle
            let _ = fs::remove_dir_all(&fw_build);
            let fw_dir = fw_build.join(format!("{}.framework/Versions/A", fw_name));
            fs_ctx::create_dir_all(fw_dir.join("Resources"))?;
            fs_ctx::copy(&dst, fw_dir.join(&fw_name))?;

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
                std::os::unix::fs::symlink("A", fw_root.join("Versions/Current"))?;
                std::os::unix::fs::symlink(
                    format!("Versions/Current/{}", fw_name),
                    fw_root.join(&fw_name),
                )?;
                std::os::unix::fs::symlink("Versions/Current/Resources", fw_root.join("Resources"))?;
            }
            #[cfg(not(unix))]
            {
                return Err("AU v3 framework builds are only supported on macOS".into());
            }

            fs_ctx::write(
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
                    suf = p.suffix,
                ),
            )?;

            {
                let mut cs_args = vec!["--force", "--sign", sign_id];
                if is_production_identity(sign_id) {
                    cs_args.extend_from_slice(&["--options", "runtime", "--timestamp"]);
                }
                cs_args.push(fw_root.to_str().unwrap());
                let status = Command::new("codesign").args(&cs_args).status()?;
                if !status.success() {
                    return Err("codesign failed for AU v3 framework".into());
                }
            }

            // Step 3: Prepare Xcode project from embedded templates
            let _ = fs::remove_dir_all(&build_dir);
            fs_ctx::create_dir_all(build_dir.join("AUExt"))?;
            fs_ctx::create_dir_all(build_dir.join("App"))?;
            fs_ctx::create_dir_all(build_dir.join("XcodeAUv3.xcodeproj"))?;

            fs_ctx::write(build_dir.join("AUExt/AudioUnitFactory.swift"), templates::au3::SWIFT_SOURCE)?;
            fs_ctx::write(build_dir.join("AUExt/BridgingHeader.h"), templates::au3::BRIDGING_HEADER)?;
            fs_ctx::write(build_dir.join("AUExt/au_shim_types.h"), templates::au3::SHIM_TYPES_H)?;
            fs_ctx::write(build_dir.join("AUExt/AUExt.entitlements"), templates::au3::APPEX_ENTITLEMENTS)?;
            fs_ctx::write(build_dir.join("App/main.m"), templates::au3::APP_MAIN_M)?;
            fs_ctx::write(build_dir.join("App/App.entitlements"), templates::au3::APP_ENTITLEMENTS)?;

            // Patch AUExt/Info.plist with plugin-specific values
            let plist_path = build_dir.join("AUExt/Info.plist");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            let ver = format!("{}.{}", now.as_secs(), now.subsec_millis());

            let plist = templates::au3::APPEX_INFO_PLIST
                .replace("AUVER", &ver)
                .replace("AUTYPE", &p.resolved_au_type())
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
            fs_ctx::write(&plist_path, plist)?;

            // Generate pbxproj (the template dir has an empty xcodeproj)
            let pbx_path = build_dir.join("XcodeAUv3.xcodeproj/project.pbxproj");
            fs_ctx::write(
                &pbx_path,
                generate_pbxproj(
                    &team_id,
                    &format!("{}.v3", p.suffix),
                    &format!("{}.v3.ext", p.suffix),
                    build_dir.join("AUExt").to_str().unwrap(),
                    fw_build.to_str().unwrap(),
                    &fw_name,
                ),
            )?;

            // Write App Info.plist from embedded template
            fs_ctx::write(build_dir.join("App/Info.plist"), templates::au3::APP_INFO_PLIST)?;

            // Step 4: xcodebuild
            eprintln!("  Building with xcodebuild...");
            // ARCHS reflects the requested slices. ONLY_ACTIVE_ARCH=NO forces
            // xcodebuild to build every listed arch regardless of host — the
            // default flips to YES in Debug and NO in Release, but we pin it
            // explicitly so dev paths (Debug) also produce the full set.
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
                // Find error lines
                for line in stdout.lines().chain(stderr.lines()) {
                    if line.contains("error:") || line.contains("BUILD FAILED") {
                        eprintln!("  {line}");
                    }
                }
                return Err(format!("xcodebuild failed for {}", p.name).into());
            }
        }

        let built_app = build_dir.join("build/Release/TruceAUv3.app");
        if !built_app.exists() {
            return Err(format!("Built app not found: {}", built_app.display()).into());
        }
    }
    Ok(())
}

/// Install pre-built AU v3 appex bundles to /Applications/ and register.
fn install_au_v3(
    config: &Config,
    plugins: &[&PluginDef],
) -> Res {
    let sign_id = config.macos.application_identity();

    for p in plugins {
        let fw_name = p.fw_name();
        let app_dir = format!("/Applications/{}.app", p.au3_app_name());
        let appex_id = format!(
            "com.{}.{}.v3.ext",
            config.vendor.id.trim_start_matches("com."),
            p.suffix
        );
        let build_dir = tmp_dir().join(format!("au_v3_build_{}", p.suffix));
        let fw_build = tmp_dir().join(format!("au_v3_fw_{}", p.suffix));
        let built_app = build_dir.join("build/Release/TruceAUv3.app");
        if !built_app.exists() {
            return Err(format!("AU v3 not built for {}. Run build first.", p.name).into());
        }

        {
            // Pre-clean. `pluginkit -e ignore` only disables the registration —
            // if `pkd` auto-discovered the build-tree appex during xcodebuild,
            // its path stays in the database and can win the next dyld load
            // race over our installed copy (which has Frameworks/ wired up
            // properly). `pluginkit -r <path>` evicts it so the subsequent
            // `-a /Applications/...` registers cleanly.
            let build_tree_appex = build_dir
                .join("build/Release/TruceAUv3.app/Contents/PlugIns/AUExt.appex");
            if build_tree_appex.exists() {
                let _ = Command::new("pluginkit")
                    .args(["-r", build_tree_appex.to_str().unwrap()])
                    .output();
            }
            let _ = Command::new("pluginkit")
                .args(["-e", "ignore", "-i", &appex_id])
                .output();
            let _ = run_sudo("rm", &["-rf", &app_dir]);

            // Install to /Applications/
            run_sudo("cp", &["-R", built_app.to_str().unwrap(), &app_dir])?;
            run_sudo("mkdir", &["-p", &format!("{app_dir}/Contents/Frameworks")])?;
            let fw_src = fw_build.join(format!("{fw_name}.framework"));
            run_sudo(
                "cp",
                &[
                    "-R",
                    fw_src.to_str().unwrap(),
                    &format!("{app_dir}/Contents/Frameworks/{fw_name}.framework"),
                ],
            )?;

            // Step 6: Sign inside-out
            let production = is_production_identity(sign_id);
            let runtime_flags: &[&str] = if production {
                &["--options", "runtime", "--timestamp"]
            } else {
                &[]
            };

            {
                let fw_path = format!("{app_dir}/Contents/Frameworks/{fw_name}.framework");
                let mut args = vec!["--force", "--sign", sign_id];
                args.extend_from_slice(runtime_flags);
                args.push(&fw_path);
                run_sudo("codesign", &args)?;
            }
            let entitlements_appex = build_dir.join("AUExt/AUExt.entitlements");
            let entitlements_app = build_dir.join("App/App.entitlements");
            {
                let appex_path = format!("{app_dir}/Contents/PlugIns/AUExt.appex");
                let ent = entitlements_appex.to_str().unwrap();
                let mut args = vec!["--force", "--sign", sign_id, "--entitlements", ent, "--generate-entitlement-der"];
                args.extend_from_slice(runtime_flags);
                args.push(&appex_path);
                run_sudo("codesign", &args)?;
            }
            {
                let ent = entitlements_app.to_str().unwrap();
                let mut args = vec!["--force", "--sign", sign_id, "--entitlements", ent, "--generate-entitlement-der"];
                args.extend_from_slice(runtime_flags);
                args.push(&app_dir);
                run_sudo("codesign", &args)?;
            }

            // Step 7: Cache bust + register
            let _ = Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
                .args(["-f", "-R", &app_dir]).output();
            let _ = run_sudo("killall", &["-9", "pkd"]);
            let _ = run_sudo("killall", &["-9", "AudioComponentRegistrar"]);
            let home = dirs::home_dir().unwrap();
            let _ = fs::remove_dir_all(home.join("Library/Caches/AudioUnitCache"));
            std::thread::sleep(std::time::Duration::from_secs(2));

            // `pluginkit -a` silently no-ops if `pkd` is still respawning
            // after the killall above, which is what happened when one
            // plugin out of a batch wouldn't show up in hosts. Retry a
            // few times, verifying via `-m -i` that the appex actually
            // appears in the registry before moving on.
            let appex_path = format!("{app_dir}/Contents/PlugIns/AUExt.appex");
            if !register_appex(&appex_path, &appex_id) {
                eprintln!(
                    "  WARNING: pluginkit did not register {appex_id}. \
                     Run `pluginkit -a \"{appex_path}\"` manually after \
                     `pkd` has settled."
                );
            }

            eprintln!("  Installed: {app_dir}");
        }
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
        // `-vv` so the output is stable across pluginkit versions; we
        // only care whether the id string appears.
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

pub(crate) fn build_and_install_au_v3(
    root: &Path,
    config: &Config,
    plugins: &[&PluginDef],
    no_build: bool,
) -> Res {
    // `cargo truce install` only needs the host arch — universal builds are
    // reserved for the packaging path.
    build_au_v3(root, config, plugins, no_build, &[MacArch::host()])?;
    install_au_v3(config, plugins)
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
