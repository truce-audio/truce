//! Integration test: the hot-reload state-preservation decision.
//!
//! `HotShell::process` makes the riskiest call in the loader - on a
//! dylib swap it either keeps the live DSP state (when the new dylib's
//! `State` fingerprint matches) or drops it through the *origin* dylib
//! and re-inits from the new one. This test reproduces that exact
//! decision at the `NativeLoader` level (deterministic, no watcher
//! timing) with real dylibs so both branches - and the subtle
//! drop-through-a-leaked-origin-dylib path - are actually executed:
//!
//! - `keep-a` -> `keep-b`: identical `CounterState` logic from two
//!   crates, so the fingerprint matches - the state must survive.
//! - `keep-a` -> `reset`: `ResetState` adds a field, so the fingerprint
//!   differs - the state must be dropped through keep-a's leaked dylib
//!   and re-initialized from `reset`.
//!
//! The fixtures surface their `counter` (advanced once per `process`)
//! through `latency`, so the test reads it back to tell "kept" from
//! "reset". Needs the three fixture cdylibs built into `target/debug`
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
    fn reload_keeps_state_on_matching_fingerprint_and_resets_on_change() {
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

        let (state, fp_a, drop_a) = loader.init_state().expect("init state from keep-a");
        assert_ne!(
            fp_a,
            truce_core::dsp_state::NO_PRESERVE,
            "CounterState derives DspState, so it must report a real fingerprint"
        );

        loader.reset(state, &AudioConfig::new(44100.0, 64));
        for _ in 0..3 {
            tick(&loader, state);
        }
        let before = loader.latency(state);
        assert_eq!(before, 3, "counter advances once per process block");

        // --- Branch 1: same-fingerprint reload (code-only edit). ---
        // keep-b is a distinct dylib file with an identical CounterState
        // layout. Reload swaps the code; the fingerprint still matches,
        // so the shell keeps the live state - mirror that here by
        // running keep-b's `process` on keep-a's `state` pointer.
        swap_in(&keep_b, &scratch);
        assert!(loader.reload(), "reload to keep-b should succeed");
        assert_eq!(
            loader.state_fingerprint(),
            Some(fp_a),
            "keep-b shares CounterState, so the fingerprint is unchanged"
        );
        let after_keep = tick(&loader, state);
        assert_eq!(
            after_keep,
            before + 1,
            "state preserved across the code-only reload (counter continued, not reset)"
        );

        // --- Branch 2: fingerprint-change reload (layout changed). ---
        // reset uses ResetState (an extra field), so its fingerprint
        // differs. The shell must drop the old state through its origin
        // dylib (keep-a, now doubly leaked) and re-init from `reset`.
        swap_in(&reset, &scratch);
        assert!(loader.reload(), "reload to reset should succeed");
        let fp_reset = loader.state_fingerprint().expect("reset fingerprint");
        assert_ne!(
            fp_reset, fp_a,
            "ResetState adds a field, so its fingerprint must differ"
        );

        // Free the old allocation through keep-a's `drop` (captured at
        // init) - this is the drop-through-leaked-origin-dylib path.
        drop_a(state);
        let (state2, _fp2, drop_b) = loader.init_state().expect("init fresh state from reset");
        let after_reset = tick(&loader, state2);
        assert_eq!(
            after_reset, 1,
            "fresh state started at 0 and advanced once (old counter was discarded)"
        );

        drop_b(state2);
        let _ = std::fs::remove_file(&scratch);
        drop(params);
    }
}
