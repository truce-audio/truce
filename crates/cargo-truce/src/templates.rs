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
