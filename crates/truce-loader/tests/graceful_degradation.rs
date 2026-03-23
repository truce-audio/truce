//! Regression test: missing dylib should not crash.
//! Bug: after cargo clean, dev shell couldn't find the debug dylib.
//! Plugin loaded with no logic, no GUI, no audio — but didn't crash.

#[cfg(feature = "shell")]
mod test {
    use truce_loader::NativeLoader;
    use std::path::PathBuf;

    #[test]
    fn missing_dylib_no_crash() {
        // Point at a nonexistent dylib.
        let path = PathBuf::from("/tmp/nonexistent_plugin_dylib.dylib");
        let loader = NativeLoader::new(path);

        // Plugin should be None (not loaded), not a crash.
        assert!(loader.plugin().is_none(), "should not load a nonexistent dylib");
    }

    #[test]
    fn corrupt_dylib_no_crash() {
        // Create a temp file that's not a valid dylib.
        let path = PathBuf::from("/tmp/truce_test_corrupt.dylib");
        std::fs::write(&path, b"not a valid dylib").ok();

        let loader = NativeLoader::new(path.clone());
        assert!(loader.plugin().is_none(), "should not load a corrupt dylib");

        // Cleanup.
        std::fs::remove_file(&path).ok();
    }
}
