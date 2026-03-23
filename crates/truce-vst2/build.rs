fn main() {
    println!("cargo:rerun-if-changed=shim/vst2_shim.c");
    println!("cargo:rerun-if-changed=shim/vst2_types.h");

    let out_dir = std::env::var("OUT_DIR").unwrap();

    cc::Build::new()
        .file("shim/vst2_shim.c")
        .flag("-std=c99")
        .flag("-fvisibility=default")
        .compile("vst2_shim");

    // Force-load all symbols from the static lib
    println!("cargo:rustc-cdylib-link-arg=-Wl,-force_load,{out_dir}/libvst2_shim.a");

    // Export VST2 entry points
    println!("cargo:rustc-cdylib-link-arg=-Wl,-exported_symbol,_VSTPluginMain");
    println!("cargo:rustc-cdylib-link-arg=-Wl,-exported_symbol,_main_macho");
}
