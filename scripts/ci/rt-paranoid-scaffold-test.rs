// Appended to a freshly scaffolded plugin's `src/lib.rs` by
// `.github/workflows/ci-rt-paranoid.yml` to confirm `cargo truce new`
// produces a plugin whose `process` is allocation-free and that the
// scaffolded `rt-paranoid` wiring actually checks. A lib unit test (not
// `tests/`), so the crate-root `enable_rt_paranoid!()` allocator applies.

#[cfg(test)]
mod rt_paranoid_scaffold_check {
    use super::*;

    #[test]
    fn process_is_allocation_free() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_no_audio_alloc, driver};

        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(30))
                .input(InputSource::Constant(0.5))
                .run()
        });
    }
}
