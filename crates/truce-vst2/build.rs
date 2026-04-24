fn main() {
    println!("cargo:rerun-if-changed=shim/vst2_shim.c");
    println!("cargo:rerun-if-changed=shim/vst2_types.h");

    let out_dir = std::env::var("OUT_DIR").unwrap();

    let mut build = cc::Build::new();
    build.file("shim/vst2_shim.c");

    if !build.get_compiler().is_like_msvc() {
        build.flag("-std=c99").flag("-fvisibility=default");
    }

    build.compile("vst2_shim");

    // `rustc-link-arg-cdylib` propagates to the downstream cdylib that
    // consumes us (per cargo issue 9562) so the C shim gets force-loaded
    // and VSTPluginMain / main_macho survive dead-stripping in the
    // consumer's plugin dylib. We can't host our own cdylib target here
    // because the exported symbols are defined by the `export_vst2!`
    // macro in the consuming crate.
    if cfg!(target_os = "macos") {
        // Force-load all symbols from the static lib
        println!("cargo:rustc-link-arg-cdylib=-Wl,-force_load,{out_dir}/libvst2_shim.a");
        // Export VST2 entry points
        println!("cargo:rustc-link-arg-cdylib=-Wl,-exported_symbol,_VSTPluginMain");
        println!("cargo:rustc-link-arg-cdylib=-Wl,-exported_symbol,_main_macho");
    } else if cfg!(target_os = "windows") {
        // On Windows, whole-archive the static lib so VST2 entry points are exported
        println!("cargo:rustc-link-arg-cdylib=/WHOLEARCHIVE:{out_dir}/vst2_shim.lib");
    }
}
