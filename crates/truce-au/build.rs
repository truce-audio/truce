fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let is_macos = target_os == "macos";
    let is_ios = target_os == "ios";
    if !is_macos && !is_ios {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();

    println!("cargo:rerun-if-changed=shim/au_v2_shim.c");
    println!("cargo:rerun-if-changed=shim/au_v2_view.m");
    println!("cargo:rerun-if-changed=shim/au_shim_common.c");
    println!("cargo:rerun-if-env-changed=TRUCE_AU_PLUGIN_ID");
    let shim_include = truce_shim_types::include_dir();
    println!(
        "cargo:rerun-if-changed={}",
        shim_include.join("au_shim_types.h").display()
    );

    // - AU v2 (.component): plain-C `AudioComponentPlugInInterface`
    //   dispatch + a per-plugin ObjC class compiled into the dylib so
    //   it appears in `__objc_classlist` (required by REAPER's
    //   `[NSBundle classNamed:]` lookup). The class name must be
    //   unique per plugin: hosts load every installed `.component`
    //   into one process, and libobjc dedupes by name - the loser's
    //   bundle then returns nil from `classNamed:` and the host
    //   thinks it has no GUI. Uniqueness comes from the
    //   `TRUCE_AU_PLUGIN_ID` env var (cargo-truce sets it from
    //   truce.toml); sanitised into an alphanumeric suffix and
    //   handed to the .m via `-DTRUCE_AU_VIEW_FACTORY_NAME=...`.
    //   Plain `cargo build` (no env) uses a default name - fine for
    //   unit tests, not for multi-plugin hosting.
    //
    // - AU v3 (.appex): the AUAudioUnit subclass + factory are
    //   compiled in Swift (templates/au3/AudioUnitFactory.swift) into
    //   the appex binary by xcodebuild during install. They read the
    //   exported `g_callbacks` / `g_descriptor` / `g_param_descriptors`
    //   / `g_num_params` symbols out of the framework dylib, so this
    //   shim's only job for v3 is to populate those globals at load
    //   time.

    let plugin_id = std::env::var("TRUCE_AU_PLUGIN_ID").unwrap_or_default();
    let sanitized: String = if plugin_id.is_empty() {
        "default".to_string()
    } else {
        plugin_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    };
    let view_factory_name = format!("TruceAUCocoaViewProxy_{sanitized}");
    // `TruceAuFixedContainer` is the NSView the AU v2 host parents
    // the editor into. Same reason as the factory above: every
    // installed `.component` ends up in one host process; without
    // a per-plugin suffix libobjc dedupes the class and the loser's
    // bundle silently breaks. The Bitcrusher / Fundsp Reverb /
    // Gain / GUI Zoo collisions REAPER warns about all flow through
    // this one class.
    let fixed_container_name = format!("TruceAuFixedContainer_{sanitized}");

    let mut build = cc::Build::new();
    build.file("shim/au_shim_common.c");
    if is_macos {
        // AU v2 only exists on macOS; the .component bundle layout
        // and AudioComponentPlugInInterface dispatch are macOS-only.
        // iOS hosts AU v3 exclusively via the App Extension shim
        // (Swift-side, compiled by xcodebuild - not from this build).
        build.file("shim/au_v2_shim.c");
        build.file("shim/au_v2_view.m");
    }

    build
        .include(&shim_include)
        .flag("-fobjc-arc")
        .flag("-fmodules")
        .flag("-fvisibility=default");
    if is_macos {
        build.flag("-mmacosx-version-min=11.0");
    } else {
        // iOS deployment target. AU v3 requires iOS 11.0+ (when AU
        // App Extensions arrived); we set 16.0 to match the current
        // Swift template's minimum.
        build.flag("-mios-version-min=16.0");
    }
    if is_macos {
        build.define("TRUCE_AU_VIEW_FACTORY_NAME", view_factory_name.as_str());
        build.define(
            "TRUCE_AU_FIXED_CONTAINER_NAME",
            fixed_container_name.as_str(),
        );
    }

    build.compile("au_shim");

    // `rustc-link-arg-cdylib` propagates to the downstream cdylib that
    // consumes us (per cargo issue 9562) so the C shim gets force-loaded
    // and AU entry symbols (g_descriptor / TruceAUFactory / etc.)
    // survive dead-stripping in the consumer's plugin dylib. We can't
    // host our own cdylib target here because the exported symbols are
    // defined by the `export_au!` macro in the consuming crate.
    println!("cargo:rustc-link-arg-cdylib=-Wl,-force_load,{out_dir}/libau_shim.a");

    // Export shim globals so the v3 appex binary (compiled separately
    // by xcodebuild) can read them out of the framework dylib at
    // runtime via dynamic symbol lookup.
    println!("cargo:rustc-link-arg-cdylib=-Wl,-exported_symbol,_g_descriptor");
    println!("cargo:rustc-link-arg-cdylib=-Wl,-exported_symbol,_g_callbacks");
    println!("cargo:rustc-link-arg-cdylib=-Wl,-exported_symbol,_g_param_descriptors");
    println!("cargo:rustc-link-arg-cdylib=-Wl,-exported_symbol,_g_num_params");
    println!("cargo:rustc-link-arg-cdylib=-Wl,-exported_symbol,_g_factory_preset_descriptors");
    println!("cargo:rustc-link-arg-cdylib=-Wl,-exported_symbol,_g_num_factory_presets");

    if is_macos {
        // AU v2 factory + v2 cocoa-view class-name lookup. Both
        // symbols come from `au_v2_*.{c,m}` which we only compile
        // on macOS; force-export only when they exist.
        println!("cargo:rustc-link-arg-cdylib=-Wl,-exported_symbol,_TruceAUFactory");
        println!(
            "cargo:rustc-link-arg-cdylib=-Wl,-exported_symbol,_truce_au_view_factory_class_name"
        );
    }

    println!("cargo:rustc-link-lib=framework=AudioToolbox");
    println!("cargo:rustc-link-lib=framework=AVFAudio");
    println!("cargo:rustc-link-lib=framework=CoreAudio");
    println!("cargo:rustc-link-lib=framework=CoreMIDI");
    println!("cargo:rustc-link-lib=framework=Foundation");
    if is_macos {
        // AppKit lives only on macOS; iOS uses UIKit (linked from
        // the Swift extension binary, not the Rust framework).
        println!("cargo:rustc-link-lib=framework=AppKit");
    }
}
