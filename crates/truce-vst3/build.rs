fn main() {
    println!("cargo:rerun-if-changed=shim/vst3_shim.cpp");

    cc::Build::new()
        .cpp(true)
        .file("shim/vst3_shim.cpp")
        .flag("-std=c++17")
        .flag("-mmacosx-version-min=10.13")
        .compile("vst3_shim");
}
