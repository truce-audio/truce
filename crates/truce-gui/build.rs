fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "macos" {
        return;
    }

    println!("cargo:rerun-if-changed=shim/macos_view.m");
    println!("cargo:rerun-if-changed=shim/macos_view.h");

    cc::Build::new()
        .file("shim/macos_view.m")
        .flag("-fobjc-arc")
        .flag("-fmodules")
        .flag("-mmacosx-version-min=11.0")
        .compile("truce_macos_view");

    println!("cargo:rustc-link-lib=framework=AppKit");
    println!("cargo:rustc-link-lib=framework=QuartzCore");
    println!("cargo:rustc-link-lib=framework=CoreGraphics");
}
