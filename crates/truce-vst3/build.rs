fn main() {
    println!("cargo:rerun-if-changed=shim/vst3_shim.cpp");

    let mut build = cc::Build::new();
    build.cpp(true).file("shim/vst3_shim.cpp");

    if build.get_compiler().is_like_msvc() {
        build.flag("/std:c++17");
    } else {
        build.flag("-std=c++17");
        if cfg!(target_os = "macos") {
            build.flag("-mmacosx-version-min=10.13");
        }
    }

    build.compile("vst3_shim");
}
