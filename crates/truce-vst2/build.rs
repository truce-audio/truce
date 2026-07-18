fn main() {
    println!("cargo:rerun-if-changed=shim/vst2_shim.c");
    println!("cargo:rerun-if-changed=shim/vst2_types.h");

    let mut build = cc::Build::new();
    build.file("shim/vst2_shim.c");

    if !build.get_compiler().is_like_msvc() {
        build.flag("-std=c99").flag("-fvisibility=default");
    }

    build.compile("vst2_shim");

    // No force-load / export-symbol / whole-archive flags: the host's
    // `VSTPluginMain` is a `#[no_mangle]` Rust trampoline (in `export_vst2!`)
    // that rustc exports on every platform. It calls the shim's core entry,
    // so cargo's `-lstatic=vst2_shim` link plus that reference pull the shim
    // object into the plugin cdylib - the same way the pure-Rust wrappers
    // (truce-clap) export their entry. A C `VSTPluginMain` would be demoted
    // to local by rustc's cdylib link on Linux and never resolve via dlsym.
}
