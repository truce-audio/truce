use std::collections::HashMap;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .parent()
        .unwrap() // examples/
        .parent()
        .unwrap() // workspace root
        .to_path_buf();
    let widgets = workspace_root.join("crates/truce-slint/ui/widgets.slint");
    let fonts_dir = workspace_root.join("fonts");

    let config = slint_build::CompilerConfiguration::new()
        .with_library_paths(HashMap::from([("truce".to_string(), widgets)]))
        .with_include_paths(vec![fonts_dir.clone()]);

    slint_build::compile_with_config("ui/main.slint", config).expect("failed to compile .slint UI");

    println!(
        "cargo:rerun-if-changed={}",
        fonts_dir.join("JetBrainsMono-Regular.ttf").display()
    );
}
