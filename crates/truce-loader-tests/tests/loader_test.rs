//! Integration test: load the `reload-fixture-keep-a` dylib (a real
//! flat-ABI logic dylib built by the workspace) and exercise the loader
//! against it - canary + symbol resolution, a passthrough `process`
//! block, and a `save_state` / `load_state` round-trip.
//!
//! The fixture's `CounterState` advances one step per `process` and is
//! surfaced through `latency` (and serialized by `save_state`), so the
//! tests can observe that a block actually ran and that state round-
//! trips. Needs `cargo build --workspace` first; skips if the dylib is
//! absent.

#[cfg(feature = "shell")]
mod test {
    use std::path::PathBuf;
    use std::sync::Arc;

    use reload_fixture_common::FxParams;
    use truce_core::AudioConfig;
    use truce_core::buffer::AudioBuffer;
    use truce_core::events::{EventList, TransportInfo};
    use truce_core::process::ProcessContext;
    use truce_loader::*;

    fn dylib_path() -> PathBuf {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop(); // crates/
        path.pop(); // workspace root
        path.push("target");
        path.push("debug");

        #[cfg(target_os = "macos")]
        path.push("libreload_fixture_keep_a.dylib");
        #[cfg(target_os = "linux")]
        path.push("libreload_fixture_keep_a.so");
        #[cfg(target_os = "windows")]
        path.push("reload_fixture_keep_a.dll");

        path
    }

    /// The loader dereferences the params pointer on every state op, so
    /// the pointer must be valid (never null) and the `Arc` must outlive
    /// the loader. Returns both so the caller keeps the `Arc` alive.
    fn params() -> (Arc<FxParams>, *const ()) {
        let params = Arc::new(FxParams::new());
        let ptr = Arc::as_ptr(&params).cast::<()>();
        (params, ptr)
    }

    #[test]
    fn load_and_verify_canary() {
        let path = dylib_path();
        if !path.exists() {
            eprintln!("skipping: fixture dylib not found at {}", path.display());
            eprintln!("build it first: cargo build --workspace");
            return;
        }

        let (_params, params_ptr) = params();
        let loader: NativeLoader = NativeLoader::new(path, params_ptr);
        assert!(loader.is_loaded(), "fixture should load");

        // Allocate state from the dylib; a fresh CounterState reports a
        // zero counter through `latency`, and no tail.
        let (state, origin) = loader.init_state().expect("init state");
        assert_eq!(loader.latency(state), 0);
        assert_eq!(loader.tail(state), 0);
        (origin.drop)(state);
    }

    #[test]
    fn process_audio() {
        let path = dylib_path();
        if !path.exists() {
            eprintln!("skipping: fixture dylib not found");
            return;
        }

        let (_params, params_ptr) = params();
        let loader: NativeLoader = NativeLoader::new(path, params_ptr);
        let (state, origin) = loader.init_state().expect("init state");
        loader.reset(state, &AudioConfig::new(44100.0, 512));

        let input_l = vec![0.5f32; 512];
        let input_r = vec![0.5f32; 512];
        let mut output_l = vec![0.0f32; 512];
        let mut output_r = vec![0.0f32; 512];

        let inputs: Vec<&[f32]> = vec![&input_l, &input_r];
        let mut outputs: Vec<&mut [f32]> = vec![&mut output_l, &mut output_r];
        let mut buffer = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs, 512) };

        let events = EventList::default();
        let transport = TransportInfo::default();
        let mut output_events = EventList::default();
        let param_fn = |_id: u32| -> f64 { 0.0 };
        let meter_fn = |_id: u32, _v: f32| {};
        let mut context = ProcessContext::new(&transport, 44100.0, 512, &mut output_events)
            .with_params(&param_fn)
            .with_meters(&meter_fn);

        let result = loader.process(state, &mut buffer, &events, &mut context);
        assert_eq!(result, ProcessStatus::Normal);

        // The fixture is a passthrough, so output mirrors input, and the
        // block advanced the counter (proving `process` actually ran).
        assert!(
            (output_l[511] - 0.5).abs() < f32::EPSILON,
            "passthrough should copy input"
        );
        assert_eq!(loader.latency(state), 1, "one block advanced the counter");
        (origin.drop)(state);
    }

    #[test]
    fn save_and_restore_state() {
        let path = dylib_path();
        if !path.exists() {
            eprintln!("skipping: fixture dylib not found");
            return;
        }

        let (_params, params_ptr) = params();
        let loader: NativeLoader = NativeLoader::new(path, params_ptr);
        let (state, origin) = loader.init_state().expect("init state");
        loader.reset(state, &AudioConfig::new(44100.0, 512));

        // Advance the counter so the saved blob carries non-default
        // state, then confirm the round-trip preserves it byte for byte.
        let input = vec![0.0f32; 64];
        let mut output = vec![0.0f32; 64];
        let inputs: Vec<&[f32]> = vec![&input];
        let mut outputs: Vec<&mut [f32]> = vec![&mut output];
        let mut buffer = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs, 64) };
        let events = EventList::default();
        let transport = TransportInfo::default();
        let mut out_events = EventList::default();
        let param_fn = |_: u32| 0.0;
        let meter_fn = |_: u32, _: f32| {};
        let mut ctx = ProcessContext::new(&transport, 44100.0, 64, &mut out_events)
            .with_params(&param_fn)
            .with_meters(&meter_fn);
        for _ in 0..7 {
            loader.process(state, &mut buffer, &events, &mut ctx);
        }

        let blob = loader.save_state(state);
        assert_eq!(loader.latency(state), 7, "counter advanced before save");

        loader
            .load_state(state, &blob)
            .expect("save/load_state round-trip should succeed");
        let blob2 = loader.save_state(state);
        assert_eq!(blob, blob2);
        assert_eq!(loader.latency(state), 7, "restored counter matches saved");
        (origin.drop)(state);
    }
}
