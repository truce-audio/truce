//! Embedded template files for AU v3 and AAX builds.
//!
//! These are compiled into the binary via `include_str!` so the tool
//! works without the `au3-template/` or `aax-template/` directories.

// ---------------------------------------------------------------------------
// AU v3 template files
// ---------------------------------------------------------------------------

pub mod au3 {
    pub const SWIFT_SOURCE: &str = include_str!("../templates/au3/AudioUnitFactory.swift");
    pub const BRIDGING_HEADER: &str = include_str!("../templates/au3/BridgingHeader.h");
    pub const APP_MAIN_M: &str = include_str!("../templates/au3/main.m");
    pub const APPEX_ENTITLEMENTS: &str = include_str!("../templates/au3/AUExt.entitlements");
    pub const APP_ENTITLEMENTS: &str = include_str!("../templates/au3/App.entitlements");
    pub const APPEX_INFO_PLIST: &str = include_str!("../templates/au3/AUExt-Info.plist");
    pub const APP_INFO_PLIST: &str = include_str!("../templates/au3/App-Info.plist");
}

// ---------------------------------------------------------------------------
// AAX template files
// ---------------------------------------------------------------------------

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
