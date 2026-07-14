fn main() {
    println!("cargo:rerun-if-changed=shim/vst3_shim.cpp");

    let mut build = cc::Build::new();
    build.cpp(true).file("shim/vst3_shim.cpp");

    if build.get_compiler().is_like_msvc() {
        build.flag("/std:c++17");
    } else {
        build.flag("-std=c++17");
        // `cfg!` in a build script reads the *host*, so cross-compiling
        // from macOS handed this macOS-only flag to e.g. the mingw g++.
        // Gate on the target via `CARGO_CFG_TARGET_OS` instead.
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
            // Match the workspace's Apple deployment floor. 10.13 was
            // honored by Xcode <= 14 but newer Xcode SDKs reject it,
            // breaking with `cstdint not found` since no matching
            // headers ship.
            build.flag("-mmacosx-version-min=11.0");
        }
    }

    build.compile("vst3_shim");
}
