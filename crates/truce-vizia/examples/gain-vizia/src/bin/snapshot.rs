//! Generate a reference GUI snapshot for gain-vizia.
//!
//! Opens a vizia window, captures the first rendered frame, saves to PNG,
//! and exits. Run with:
//!
//!     cargo run -p gain-vizia --bin snapshot

fn main() {
    // gain-vizia -> examples -> truce-vizia -> crates -> workspace root
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("screenshots")
        .join("gain_vizia_default.png");

    std::fs::create_dir_all(path.parent().unwrap()).ok();

    eprintln!("Capturing snapshot to: {}", path.display());

    truce_vizia::snapshot::capture_snapshot(
        (400, 300),
        path.to_str().unwrap(),
        gain_vizia::gain_vizia_ui,
    );
}
