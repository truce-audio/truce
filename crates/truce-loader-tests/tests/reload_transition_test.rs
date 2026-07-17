//! Integration test: the hot-reload state carry-over.
//!
//! On a dylib swap `HotShell::process` carries the live DSP state into
//! the new code by a save / load round-trip through the *origin* dylib -
//! never by reinterpreting the old bytes under the new `State` layout
//! (UB when a same-size field type or order changed). This test
//! reproduces that exact sequence at the `NativeLoader` level
//! (deterministic, no watcher timing) with real dylibs, so both the
//! carry-over and the fresh-init fallback - and the subtle
//! serialize / drop through a leaked origin dylib - are actually run:
//!
//! - `keep-a` -> `keep-b`: identical `CounterLogic` from two crates
//!   sharing a `save_state` / `load_state` format, so the count carries
//!   over.
//! - `keep-b` -> `reset`: `ResetLogic` defines no `load_state`, so the
//!   carried blob can't be restored and the state starts fresh.
//!
//! The fixtures surface their `counter` (advanced once per `process`)
//! through `latency`, so the test reads it back to tell "carried" from
//! "fresh". Needs the three fixture cdylibs built into `target/debug`
//! (`cargo build --workspace`); skips cleanly if they are absent.

#[cfg(feature = "shell")]
mod test {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use reload_fixture_common::FxParams;
    use truce_core::AudioConfig;
    use truce_core::buffer::AudioBuffer;
    use truce_core::events::{EventList, TransportInfo};
    use truce_core::process::ProcessContext;
    use truce_loader::NativeLoader;

    /// Unique tag per scratch dylib so two test runs (or two `#[test]`
    /// fns in parallel) never share a temp path.
    static SCRATCH_ID: AtomicU64 = AtomicU64::new(0);

    fn dylib_file(stem: &str) -> String {
        #[cfg(target_os = "macos")]
        return format!("lib{stem}.dylib");
        #[cfg(target_os = "linux")]
        return format!("lib{stem}.so");
        #[cfg(target_os = "windows")]
        return format!("{stem}.dll");
    }

    fn fixture(stem: &str) -> PathBuf {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop(); // crates/
        path.pop(); // workspace root
        path.push("target");
        path.push("debug");
        path.push(dylib_file(stem));
        path
    }

    /// A fresh temp path carrying the platform dylib extension, so the
    /// loader's `copy_versioned` keeps the suffix and dlopen accepts it.
    fn scratch_path() -> PathBuf {
        let id = SCRATCH_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(dylib_file(&format!("truce_reload_scratch_{id}")))
    }

    /// Run one silent `process` block against `state` and return the
    /// counter the fixture reports through `latency`. The loader already
    /// holds the shared params pointer, so callers pass only `state`.
    fn tick(loader: &NativeLoader, state: *mut ()) -> u32 {
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

        loader.process(state, &mut buffer, &events, &mut ctx);
        loader.latency(state)
    }

    /// Copy `src` over `dst`, replacing the dylib the loader watches.
    fn swap_in(src: &std::path::Path, dst: &std::path::Path) {
        std::fs::copy(src, dst).expect("copy fixture dylib over scratch path");
    }

    #[test]
    fn reload_carries_state_via_save_load_and_falls_back_to_fresh_init() {
        let (keep_a, keep_b, reset) = (
            fixture("reload_fixture_keep_a"),
            fixture("reload_fixture_keep_b"),
            fixture("reload_fixture_reset"),
        );
        if !keep_a.exists() || !keep_b.exists() || !reset.exists() {
            eprintln!("skipping: reload fixtures not built; run `cargo build --workspace`");
            return;
        }

        let scratch = scratch_path();
        swap_in(&keep_a, &scratch);

        // The params pointer the loader hands to every state op; must
        // outlive the loader (it dereferences it on init / reset).
        let params = Arc::new(FxParams::new());
        let params_ptr = Arc::as_ptr(&params).cast::<()>();

        let mut loader: NativeLoader = NativeLoader::new(scratch.clone(), params_ptr);
        assert!(loader.is_loaded(), "keep-a should load");
        assert!(
            loader.preserve_dsp_state(),
            "CounterLogic keeps the default PRESERVE_DSP_STATE = true"
        );

        let (state, origin_a) = loader.init_state().expect("init state from keep-a");

        loader.reset(state, &AudioConfig::new(44100.0, 64));
        for _ in 0..3 {
            tick(&loader, state);
        }
        let before = loader.latency(state);
        assert_eq!(before, 3, "counter advances once per process block");

        // --- Branch 1: reload carries state via save / load. ---
        // keep-b is a distinct dylib sharing CounterLogic's blob format.
        // Mirror the shell exactly: serialize the live state through the
        // ORIGIN dylib (keep-a, still mapped), reload, then init fresh
        // under the new code and restore the carried bytes into it.
        swap_in(&keep_b, &scratch);
        assert!(loader.reload(), "reload to keep-b should succeed");
        let carried = (origin_a.save)(state.cast_const());
        assert!(!carried.is_empty(), "CounterLogic serializes its counter");
        (origin_a.drop)(state);
        let (state_b, origin_b) = loader.init_state().expect("init fresh state from keep-b");
        loader
            .load_state(state_b, &carried)
            .expect("keep-b restores CounterState from the carried blob");
        let after_keep = tick(&loader, state_b);
        assert_eq!(
            after_keep,
            before + 1,
            "count carried across the reload (continued from 3, not reset)"
        );

        // --- Branch 2: reload whose logic can't restore the blob. ---
        // `reset` defines no `load_state`, so restoring the carried bytes
        // is a silent no-op and the fresh state starts at 0 - the sound
        // fallback. Serialize + drop through keep-b's origin dylib.
        let carried_b = (origin_b.save)(state_b.cast_const());
        swap_in(&reset, &scratch);
        assert!(loader.reload(), "reload to reset should succeed");
        (origin_b.drop)(state_b);
        let (state_c, origin_c) = loader.init_state().expect("init fresh state from reset");
        loader
            .load_state(state_c, &carried_b)
            .expect("reset's default load_state accepts any blob as a no-op");
        let after_reset = tick(&loader, state_c);
        assert_eq!(
            after_reset, 1,
            "fresh state started at 0 and advanced once (carried count discarded)"
        );

        (origin_c.drop)(state_c);
        let _ = std::fs::remove_file(&scratch);
        drop(params);
    }
}
