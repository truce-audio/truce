//! Embedded template files for AU v3 and AAX builds.
//!
//! These are compiled into the binary via `include_str!` so the tool
//! works without the `au3-template/` or `aax-template/` directories.

// ---------------------------------------------------------------------------
// AU v3 template files
// ---------------------------------------------------------------------------

// AU is macOS-only. Gating the module silences dead-code warnings on
// other platforms for the embedded `include_str!` constants.
#[cfg(target_os = "macos")]
pub mod au3 {
    pub use truce_shim_types::AU_SHIM_TYPES_H as SHIM_TYPES_H;
    pub const SWIFT_SOURCE: &str = include_str!("../templates/au3/AudioUnitFactory.swift");
    pub const BRIDGING_HEADER: &str = include_str!("../templates/au3/BridgingHeader.h");
    pub const APP_MAIN_M: &str = include_str!("../templates/au3/main.m");
    pub const APPEX_ENTITLEMENTS: &str = include_str!("../templates/au3/AUExt.entitlements");
    pub const APP_ENTITLEMENTS: &str = include_str!("../templates/au3/App.entitlements");
    pub const APPEX_INFO_PLIST: &str = include_str!("../templates/au3/AUExt-Info.plist");
    pub const APP_INFO_PLIST: &str = include_str!("../templates/au3/App-Info.plist");

    /// Values substituted into `APPEX_INFO_PLIST` by
    /// [`render_appex_info_plist`]. One struct shared by the macOS and
    /// iOS appex paths so a new placeholder added to the template forces
    /// a struct-field update — making "iOS path substitutes it, macOS
    /// path forgets" a compile error rather than a runtime
    /// `(null) platform` bundle rejection.
    pub struct AppexPlistValues<'a> {
        pub au_name: &'a str,
        pub au_type: &'a str,
        pub au_sub: &'a str,
        pub au_mfr: &'a str,
        pub au_tag: &'a str,
        pub au_ver: &'a str,
        pub min_os: &'a str,
        pub supported_platform: &'a str,
        /// `$(...)` xcodebuild-style tokens. `Some` on the iOS path
        /// (swiftc compiles the appex directly, so we substitute
        /// ourselves); `None` on the macOS path (xcodebuild expands
        /// them from the pbxproj's `PRODUCT_BUNDLE_IDENTIFIER` etc.
        /// at build time).
        pub xcode_tokens: Option<XcodeTokens<'a>>,
    }

    pub struct XcodeTokens<'a> {
        pub executable_name: &'a str,
        pub bundle_id: &'a str,
        pub package_type: &'a str,
        pub module_name: &'a str,
    }

    /// Render `APPEX_INFO_PLIST` against `values`. After substitution,
    /// asserts every placeholder we tried to replace is actually gone —
    /// catches typos where the renderer says `MINIOS` but the template
    /// was renamed to `MIN_OS` (or vice versa) before they ship as a
    /// literal token in the bundle.
    pub fn render_appex_info_plist(values: &AppexPlistValues<'_>) -> String {
        let mut subs: Vec<(&str, &str)> = vec![
            ("AUNAME", values.au_name),
            ("AUTYPE", values.au_type),
            ("AUSUB", values.au_sub),
            ("AUMFR", values.au_mfr),
            ("AUTAG", values.au_tag),
            ("AUVER", values.au_ver),
            ("MINIOS", values.min_os),
            ("SUPPORTEDPLAT", values.supported_platform),
        ];
        if let Some(x) = &values.xcode_tokens {
            subs.extend([
                ("$(EXECUTABLE_NAME)", x.executable_name),
                ("$(PRODUCT_BUNDLE_IDENTIFIER)", x.bundle_id),
                ("$(PRODUCT_BUNDLE_PACKAGE_TYPE)", x.package_type),
                ("$(PRODUCT_MODULE_NAME)", x.module_name),
            ]);
        }
        let mut plist = APPEX_INFO_PLIST.to_string();
        for (placeholder, value) in &subs {
            plist = plist.replace(placeholder, value);
        }
        for (placeholder, _) in &subs {
            assert!(
                !plist.contains(placeholder),
                "appex Info.plist still contains `{placeholder}` after substitution; \
                 template and renderer disagree on token spelling",
            );
        }
        plist
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn macos_values() -> AppexPlistValues<'static> {
            AppexPlistValues {
                au_name: "Acme: Tremolo",
                au_type: "aufx",
                au_sub: "Trem",
                au_mfr: "Acme",
                au_tag: "Effect",
                au_ver: "1",
                min_os: "13.0",
                supported_platform: "MacOSX",
                xcode_tokens: None,
            }
        }

        fn ios_values() -> AppexPlistValues<'static> {
            AppexPlistValues {
                au_name: "Tremolo",
                au_type: "aufx",
                au_sub: "Trem",
                au_mfr: "Acme",
                au_tag: "Effect",
                au_ver: "1",
                min_os: "15.0",
                supported_platform: "iPhoneOS",
                xcode_tokens: Some(XcodeTokens {
                    executable_name: "AUExt",
                    bundle_id: "com.acme.tremolo.AUExt",
                    package_type: "XPC!",
                    module_name: "AUExt",
                }),
            }
        }

        #[test]
        fn macos_render_substitutes_platform_and_min_os() {
            // Direct regression test for the (null)-platform xcodebuild
            // failure: the appex's CFBundleSupportedPlatforms must be
            // `MacOSX`, not the placeholder `SUPPORTEDPLAT`, and
            // MinimumOSVersion must match the pbxproj's
            // MACOSX_DEPLOYMENT_TARGET (13.0).
            let plist = render_appex_info_plist(&macos_values());
            assert!(plist.contains("<string>MacOSX</string>"));
            assert!(plist.contains("<string>13.0</string>"));
            assert!(!plist.contains("SUPPORTEDPLAT"));
            assert!(!plist.contains("MINIOS"));
            // macOS path leaves Xcode tokens for xcodebuild to expand.
            assert!(plist.contains("$(PRODUCT_BUNDLE_IDENTIFIER)"));
        }

        #[test]
        fn ios_render_substitutes_xcode_tokens() {
            let plist = render_appex_info_plist(&ios_values());
            assert!(plist.contains("<string>iPhoneOS</string>"));
            assert!(plist.contains("<string>15.0</string>"));
            assert!(plist.contains("com.acme.tremolo.AUExt"));
            assert!(!plist.contains("$(PRODUCT_BUNDLE_IDENTIFIER)"));
            assert!(!plist.contains("$(EXECUTABLE_NAME)"));
        }

        #[test]
        fn render_substitutes_audio_component_fields() {
            let plist = render_appex_info_plist(&macos_values());
            assert!(plist.contains("<string>aufx</string>"));
            assert!(plist.contains("<string>Trem</string>"));
            assert!(plist.contains("<string>Acme</string>"));
            assert!(plist.contains("<string>Effect</string>"));
            assert!(plist.contains("<string>Acme: Tremolo</string>"));
        }
    }
}

// ---------------------------------------------------------------------------
// AU v3 iOS container app — Swift source for the .app stub
// ---------------------------------------------------------------------------

// iOS install is macOS-only (Xcode + simctl). Gating the module
// silences dead-code warnings on Linux / Windows.
#[cfg(target_os = "macos")]
pub mod au_ios {
    /// The container app's full `AppMain.swift`: imports, the
    /// `private let log` global, free helper functions, the
    /// `UIFont` convenience extension, and the
    /// `@UIApplicationMain class AppDelegate` that hosts the
    /// editor, audio engine, and Core MIDI bridge. Carries
    /// literal `{app_name}` / `{vendor_name}` / `{description}` /
    /// `{vendor_url}` placeholders that `render_app_main_swift`
    /// substitutes per install.
    pub const APP_MAIN: &str = include_str!("../templates/au_ios/AppMain.swift");
}

// ---------------------------------------------------------------------------
// AAX template files
// ---------------------------------------------------------------------------

// AAX is macOS / Windows only — Avid's SDK ships no Linux libs and Pro
// Tools doesn't run on Linux. Gating the module keeps Linux from
// spuriously warning about unused `include_str!` constants.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod aax {
    pub const CMAKE_LISTS: &str = include_str!("../templates/aax/CMakeLists.txt");
    pub const BRIDGE_CPP: &str = include_str!("../templates/aax/TruceAAX_Bridge.cpp");
    pub const BRIDGE_H: &str = include_str!("../templates/aax/TruceAAX_Bridge.h");
    pub const DESCRIBE_CPP: &str = include_str!("../templates/aax/TruceAAX_Describe.cpp");
    pub const GUI_CPP: &str = include_str!("../templates/aax/TruceAAX_GUI.cpp");
    pub const GUI_H: &str = include_str!("../templates/aax/TruceAAX_GUI.h");
    pub const PARAMETERS_CPP: &str = include_str!("../templates/aax/TruceAAX_Parameters.cpp");
    pub const PARAMETERS_H: &str = include_str!("../templates/aax/TruceAAX_Parameters.h");
    pub const INFO_PLIST_IN: &str = include_str!("../templates/aax/Info.plist.in");
    pub const BRIDGE_HEADER: &str = include_str!("../templates/aax/truce_aax_bridge.h");
}
