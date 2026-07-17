//! Integration test: state-save entry points reconcile a hot-reload swap.
//!
//! The carry-over reconciliation runs in `HotShell::process`, but a host
//! `save_state` can land between the watcher's dylib swap and the next
//! audio block. If it ran the newly loaded dylib's `save_state` over the
//! previous dylib's allocation it would read old bytes under the new
//! layout (UB). This drives a real `HotShell` through a watcher swap to a
//! layout-changed dylib and asserts `save_state` - called before any
//! `process` reconciles - still serializes the live state through its
//! ORIGIN dylib (non-empty and correct) rather than the new dylib's
//! (empty) one. Needs the fixture cdylibs in `target/debug`
//! (`cargo build --workspace`); skips cleanly if they are absent.

#[cfg(feature = "shell")]
mod test {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use reload_fixture_common::FxParams;
    use truce_core::AudioConfig;
    use truce_core::buffer::AudioBuffer;
    use truce_core::events::{EventList, TransportInfo};
    use truce_core::plugin::PluginRuntime;
    use truce_core::process::ProcessContext;
    use truce_loader::shell::HotShell;

    /// Unique tag per scratch dylib so parallel runs never share a path.
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

    fn scratch_path() -> PathBuf {
        let id = SCRATCH_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(dylib_file(&format!("truce_shell_save_scratch_{id}")))
    }

    /// Advance the fixture's counter one block and return it (the fixture
    /// surfaces the counter through `latency`, refreshed by `process`).
    fn tick(shell: &mut HotShell<FxParams>) -> u32 {
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
        shell.process(&mut buffer, &events, &mut ctx);
        shell.latency()
    }

    #[test]
    fn save_state_between_swap_and_process_uses_origin_dylib() {
        let (keep_a, reset) = (
            fixture("reload_fixture_keep_a"),
            fixture("reload_fixture_reset"),
        );
        if !keep_a.exists() || !reset.exists() {
            eprintln!("skipping: reload fixtures not built; run `cargo build --workspace`");
            return;
        }

        let scratch = scratch_path();
        std::fs::copy(&keep_a, &scratch).expect("seed scratch with keep-a");

        let mut shell = HotShell::new(FxParams::new(), scratch.clone());
        shell.reset(&AudioConfig::new(44100.0, 64));
        for _ in 0..3 {
            tick(&mut shell);
        }
        assert_eq!(shell.latency(), 3, "counter advances once per block");

        // Baseline: no swap yet, save_state serializes the counter.
        let before = shell.save_state();
        assert_eq!(
            before,
            3u64.to_le_bytes(),
            "keep-a save_state serializes the counter"
        );

        // Swap to the layout-changed `reset` dylib (ResetLogic defines no
        // save_state) and let the watcher reload. `fs::copy` preserves the
        // source mtime on macOS, so bump it to now or the watcher's
        // mtime-advance check would skip the swap. No `process` runs, so
        // the shell has not reconciled when save_state is called next.
        std::fs::copy(&reset, &scratch).expect("swap scratch to reset");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&scratch)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()))
            .expect("bump scratch mtime so the watcher notices the swap");
        std::thread::sleep(Duration::from_millis(2500));

        // The regression: save_state must serialize through keep-a (the
        // origin of the live allocation), not the freshly loaded reset
        // dylib whose save_state would return empty over the old bytes.
        let after = shell.save_state();
        assert_eq!(
            after,
            3u64.to_le_bytes(),
            "save_state after the swap must use the origin dylib (got {} bytes)",
            after.len()
        );

        // Confirm the swap was actually live during the save above: one
        // reconciling block now re-inits under `reset` (fresh counter),
        // which it could only do if keep-a was no longer the loaded code.
        assert_eq!(
            tick(&mut shell),
            1,
            "process should have reconciled to the reset dylib (fresh state)"
        );

        drop(shell);
        let _ = std::fs::remove_file(&scratch);
    }
}
