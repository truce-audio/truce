use std::collections::HashMap;
use std::path::PathBuf;

fn main() {
    // Locate the truce-slint widget library relative to this crate.
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ui_path = manifest_dir
        .parent().unwrap()   // examples/
        .parent().unwrap()   // workspace root
        .join("crates/truce-slint/ui/widgets.slint");

    let config = slint_build::CompilerConfiguration::new()
        .with_library_paths(HashMap::from([
            ("truce".to_string(), ui_path),
        ]));
    slint_build::compile_with_config("ui/main.slint", config)
        .expect("failed to compile .slint UI");
}
