//! Integration test: load the gain-hot-logic dylib and exercise it.

#[cfg(feature = "shell")]
mod test {
    use std::path::PathBuf;
    use truce_loader::*;
    use truce_core::buffer::AudioBuffer;
    use truce_core::events::{EventList, TransportInfo};
    use truce_core::process::ProcessContext;

    fn dylib_path() -> PathBuf {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop(); // crates/
        path.pop(); // workspace root
        path.push("target");
        path.push("debug");

        #[cfg(target_os = "macos")]
        path.push("libgain_hot_logic.dylib");
        #[cfg(target_os = "linux")]
        path.push("libgain_hot_logic.so");
        #[cfg(target_os = "windows")]
        path.push("gain_hot_logic.dll");

        path
    }

    #[test]
    fn load_and_verify_canary() {
        let path = dylib_path();
        if !path.exists() {
            eprintln!("skipping: dylib not found at {}", path.display());
            eprintln!("build it first: cargo build -p gain-hot-logic");
            return;
        }

        let loader = NativeLoader::new(path);
        let plugin = loader.plugin().expect("plugin should be loaded");

        // Verify we got a real plugin (not the probe).
        assert_eq!(plugin.latency(), 0);
        assert_eq!(plugin.tail(), 0);
    }

    #[test]
    fn process_audio() {
        let path = dylib_path();
        if !path.exists() {
            eprintln!("skipping: dylib not found");
            return;
        }

        let mut loader = NativeLoader::new(path);
        let plugin = loader.plugin_mut().expect("plugin should be loaded");
        plugin.reset(44100.0, 512);

        // Create test audio buffers.
        let input_l = vec![0.5f32; 512];
        let input_r = vec![0.5f32; 512];
        let mut output_l = vec![0.0f32; 512];
        let mut output_r = vec![0.0f32; 512];

        let inputs: Vec<&[f32]> = vec![&input_l, &input_r];
        let mut outputs: Vec<&mut [f32]> = vec![&mut output_l, &mut output_r];

        let mut buffer = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs, 512) };

        // Create process context (0 dB gain = passthrough).
        let events = EventList::new();
        let transport = TransportInfo::default();
        let mut output_events = EventList::new();
        let param_fn = |_id: u32| -> f64 { 0.0 }; // 0 dB
        let meter_fn = |_id: u32, _v: f32| {};
        let mut context = ProcessContext::new(&transport, 44100.0, 512, &mut output_events)
            .with_params(&param_fn)
            .with_meters(&meter_fn);

        let result = plugin.process(&mut buffer, &events, &mut context);
        assert_eq!(result, ProcessStatus::Normal);

        // Output should be close to input (gain smoothing may not
        // reach exactly 1.0 in 512 samples from the default of 1.0
        // with target of 1.0, so it should be very close).
        assert!(output_l[511].abs() > 0.0, "output should be non-zero");
    }

    #[test]
    fn compute_layout() {
        let path = dylib_path();
        if !path.exists() {
            eprintln!("skipping: dylib not found");
            return;
        }

        let loader = NativeLoader::new(path);
        let plugin = loader.plugin().expect("plugin should be loaded");

        let layout = plugin.layout();
        assert!(layout.width > 0);
        assert!(layout.height > 0);
        assert!(!layout.widgets.is_empty(), "layout should have at least one widget");
    }

    #[test]
    fn save_and_restore_state() {
        let path = dylib_path();
        if !path.exists() {
            eprintln!("skipping: dylib not found");
            return;
        }

        let mut loader = NativeLoader::new(path);
        let plugin = loader.plugin_mut().expect("plugin should be loaded");

        let state = plugin.save_state();
        assert_eq!(state.len(), 8); // f64 = 8 bytes

        // Restore into a fresh instance.
        plugin.load_state(&state);
        let state2 = plugin.save_state();
        assert_eq!(state, state2);
    }
}
