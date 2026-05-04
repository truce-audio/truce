//! Test utilities for truce plugins.
//!
//! Two layers:
//!
//! - **Audio runs** — built on top of [`truce_driver::PluginDriver`].
//!   Re-exported here so plugin tests have one crate to depend on.
//!   Use the [`driver!`] macro for ergonomic builder construction
//!   (it wires `manifest_dir` from the calling crate's
//!   `CARGO_MANIFEST_DIR`, so `state_file` paths resolve correctly).
//!   Assertions live in [`assertions`].
//! - **Static plugin checks** — `assert_state_round_trip`,
//!   `assert_has_editor`, AU `FourCC`, bus config, param defaults, GUI
//!   lifecycle, etc. These don't render audio, just instantiate the
//!   plugin and inspect.
//!
//! # Usage
//!
//! Add to your plugin crate's `[dev-dependencies]`:
//! ```toml
//! [dev-dependencies]
//! truce-test = { workspace = true }
//! ```
//!
//! ```ignore
//! use truce_test::{assertions, driver, InputSource};
//! use std::time::Duration;
//!
//! #[test]
//! fn passthrough() {
//!     let result = driver!(MyPlugin)
//!         .duration(Duration::from_millis(100))
//!         .input(InputSource::Constant(0.5))
//!         .run();
//!     assertions::assert_nonzero(&result);
//!     assertions::assert_no_nans(&result);
//!     assertions::assert_peak_below(&result, 1.0);
//! }
//! ```

use truce_core::export::PluginExport;
use truce_core::state;
use truce_params::Params;

// ---------------------------------------------------------------------------
// Driver re-exports + ergonomic macro
// ---------------------------------------------------------------------------

pub use truce_driver::{
    CaptureSpec, DriverResult, InputSource, MeterCapture, MeterReadings, PluginDriver, Script,
    SetupContext, TransportSpec,
};

pub mod assertions;

/// Re-export of [`truce_core::editor::for_test_params`]
/// for plugin authors who want to drive snapshot tests directly
/// without the `assert_screenshot!` macro.
pub use truce_core::editor::for_test_params;

/// Construct a [`PluginDriver`] for the given plugin type, with
/// `manifest_dir` wired to the calling crate's `CARGO_MANIFEST_DIR`.
/// That lets `.state_file("test_states/foo.pluginstate")` resolve
/// against the crate's own directory regardless of where `cargo
/// test` was launched.
///
/// ```ignore
/// truce_test::driver!(MyPlugin)
///     .duration(Duration::from_millis(100))
///     .state_file("test_states/preset.pluginstate")
///     .run();
/// ```
#[macro_export]
macro_rules! driver {
    ($plugin:ty $(,)?) => {
        $crate::PluginDriver::<$plugin>::new().manifest_dir(env!("CARGO_MANIFEST_DIR"))
    };
}

// ---------------------------------------------------------------------------
// Static plugin checks (no audio render)
// ---------------------------------------------------------------------------

/// Assert state save/load round-trips correctly.
///
/// Saves state, creates a new instance, loads state, and verifies
/// all parameter values match.
///
/// # Panics
///
/// Panics if `restore_plugin` fails, any parameter id is missing
/// after restore (renamed / renumbered between save and load), or
/// any restored value differs from the source by more than `1e-4`.
pub fn assert_state_round_trip<P: PluginExport>() {
    let plugin = P::create();
    let blob = state::snapshot_plugin(&plugin);

    let mut plugin2 = P::create();
    state::restore_plugin(&mut plugin2, &blob).expect("restore_plugin failed");

    let param_infos = plugin.params().param_infos();
    for pi in &param_infos {
        // `get_plain` returns `None` if the param id was dropped during
        // round-trip — for example, a plugin update that renumbered
        // params. We surface that as the assertion failure rather than
        // an `.unwrap()` panic that would point at the wrong line.
        let v1 = plugin.params().get_plain(pi.id).unwrap_or_else(|| {
            panic!(
                "param {} ({}) missing from source plugin after restore_plugin — \
                 the param id is no longer registered",
                pi.id, pi.name
            )
        });
        let v2 = plugin2.params().get_plain(pi.id).unwrap_or_else(|| {
            panic!(
                "param {} ({}) was lost during state round-trip — \
                 saved-state blob references an id that the freshly-built plugin \
                 doesn't expose. Either the param was renamed/renumbered or \
                 the deserializer is dropping it.",
                pi.id, pi.name
            )
        });
        assert!(
            (v1 - v2).abs() < 0.0001,
            "Param {} ({}) mismatch: {v1} vs {v2}",
            pi.id,
            pi.name
        );
    }
}

/// Assert the plugin has a working editor with valid dimensions.
///
/// # Panics
///
/// Panics if `Plugin::editor()` returns `None` or the editor's
/// reported size has a zero dimension.
pub fn assert_has_editor<P: PluginExport>() {
    let mut plugin = P::create();
    let editor = plugin.editor();
    assert!(editor.is_some(), "Plugin::editor() returned None");
    let editor = editor.unwrap();
    let (w, h) = editor.size();
    assert!(w > 0 && h > 0, "Editor size is zero: {w}x{h}");
}

/// Assert `plugin_info`!() returns valid metadata.
///
/// # Panics
///
/// Panics if any string field is empty or any FourCC code is all
/// zeros.
pub fn assert_valid_info<P: PluginExport>() {
    let info = P::info();
    assert!(!info.name.is_empty(), "Plugin name is empty");
    assert!(!info.vendor.is_empty(), "Vendor is empty");
    assert!(!info.version.is_empty(), "Version is empty");
    assert!(!info.clap_id.is_empty(), "CLAP ID is empty");
    assert!(!info.vst3_id.is_empty(), "VST3 ID is empty");
    assert!(info.au_type != [0; 4], "AU type is zero");
    assert!(info.fourcc != [0; 4], "FourCC is zero");
    assert!(info.au_manufacturer != [0; 4], "AU manufacturer is zero");
}

// ---------------------------------------------------------------------------
// AU metadata tests
// ---------------------------------------------------------------------------

/// Assert AU type codes are valid 4-char ASCII.
///
/// Catches the `FourCharCode` endianness bug (big-endian on ARM64).
///
/// # Panics
///
/// Panics if any byte of `au_type`, `fourcc`, or `au_manufacturer`
/// isn't a printable ASCII glyph.
pub fn assert_au_type_codes_ascii<P: PluginExport>() {
    let info = P::info();
    for (label, code) in [
        ("au_type", info.au_type),
        ("fourcc", info.fourcc),
        ("au_manufacturer", info.au_manufacturer),
    ] {
        for (i, &byte) in code.iter().enumerate() {
            assert!(
                byte.is_ascii_graphic(),
                "{label}[{i}] is not printable ASCII: 0x{byte:02x} (full: {:?})",
                std::str::from_utf8(&code).unwrap_or("??")
            );
        }
    }
}

/// Assert AU `FourCharCode` round-trips through big-endian u32.
///
/// This is the encoding used by `AudioComponentDescription` on macOS.
///
/// # Panics
///
/// Panics if the big-endian pack/unpack of any FourCharCode
/// doesn't reproduce the original byte sequence.
pub fn assert_fourcc_roundtrip<P: PluginExport>() {
    let info = P::info();
    for (label, code) in [
        ("au_type", info.au_type),
        ("fourcc", info.fourcc),
        ("au_manufacturer", info.au_manufacturer),
    ] {
        let packed = (u32::from(code[0]) << 24)
            | (u32::from(code[1]) << 16)
            | (u32::from(code[2]) << 8)
            | u32::from(code[3]);
        let unpacked = [
            (packed >> 24) as u8,
            (packed >> 16) as u8,
            (packed >> 8) as u8,
            packed as u8,
        ];
        assert_eq!(code, unpacked, "{label} FourCharCode round-trip failed");
    }
}

/// Assert bus config is correct for an effect (has inputs and outputs).
///
/// # Panics
///
/// Panics if no bus layouts are defined, or the first layout
/// reports zero input or output channels.
pub fn assert_bus_config_effect<P: PluginExport>() {
    let layouts = P::bus_layouts();
    assert!(!layouts.is_empty(), "No bus layouts defined");
    let layout = &layouts[0];
    let inputs = layout.total_input_channels();
    let outputs = layout.total_output_channels();
    assert!(
        inputs > 0,
        "Effect should have input channels, got {inputs}"
    );
    assert!(
        outputs > 0,
        "Effect should have output channels, got {outputs}"
    );
}

/// Assert bus config is correct for an instrument (no inputs, has outputs).
///
/// Catches the `GarageBand` `SupportedNumChannels` bug — instruments must
/// report 0 input channels for AU hosts to show them.
///
/// # Panics
///
/// Panics if no bus layouts are defined, the first layout reports
/// any input channels, or it reports zero output channels.
pub fn assert_bus_config_instrument<P: PluginExport>() {
    let layouts = P::bus_layouts();
    assert!(!layouts.is_empty(), "No bus layouts defined");
    let layout = &layouts[0];
    let inputs = layout.total_input_channels();
    let outputs = layout.total_output_channels();
    assert_eq!(
        inputs, 0,
        "Instrument should have 0 input channels, got {inputs}"
    );
    assert!(
        outputs > 0,
        "Instrument should have output channels, got {outputs}"
    );
}

// ---------------------------------------------------------------------------
// GUI lifecycle tests
// ---------------------------------------------------------------------------

/// Assert editor can be created multiple times without issues.
///
/// Catches lifecycle bugs where create/drop leaves state dirty.
///
/// # Panics
///
/// Panics if `editor()` returns `None` on first or second creation,
/// the first editor reports a zero dimension, or the size differs
/// between consecutive `editor()` calls.
pub fn assert_editor_lifecycle<P: PluginExport>() {
    let mut plugin = P::create();

    // First creation
    let editor1 = plugin.editor();
    assert!(editor1.is_some(), "First editor() returned None");
    let (w1, h1) = editor1.as_ref().unwrap().size();
    assert!(w1 > 0 && h1 > 0, "First editor size is zero: {w1}x{h1}");
    drop(editor1);

    // Second creation after drop
    let editor2 = plugin.editor();
    assert!(
        editor2.is_some(),
        "Second editor() returned None after drop"
    );
    let (w2, h2) = editor2.as_ref().unwrap().size();
    assert_eq!(
        (w1, h1),
        (w2, h2),
        "Editor size changed between creates: ({w1},{h1}) vs ({w2},{h2})"
    );
}

/// Assert editor size is consistent across multiple calls.
///
/// # Panics
///
/// Panics if `editor()` returns `None` or the reported size differs
/// across three back-to-back `size()` calls.
pub fn assert_editor_size_consistent<P: PluginExport>() {
    let mut plugin = P::create();
    let editor = plugin.editor();
    assert!(editor.is_some(), "editor() returned None");
    let editor = editor.unwrap();
    let (w1, h1) = editor.size();
    let (w2, h2) = editor.size();
    let (w3, h3) = editor.size();
    assert_eq!((w1, h1), (w2, h2), "Editor size inconsistent: call 1 vs 2");
    assert_eq!((w2, h2), (w3, h3), "Editor size inconsistent: call 2 vs 3");
}

// ---------------------------------------------------------------------------
// Parameter tests
// ---------------------------------------------------------------------------

/// Assert all parameter default values match their declared defaults.
///
/// # Panics
///
/// Panics if `get_plain` returns `None` for an id that has a
/// `ParamInfo` entry (derive-macro inconsistency), or if the current
/// plain value differs from `default_plain` by more than `1e-4`.
pub fn assert_param_defaults_match<P: PluginExport>() {
    let plugin = P::create();
    let infos = plugin.params().param_infos();
    for pi in &infos {
        let current = plugin.params().get_plain(pi.id).unwrap_or_else(|| {
            panic!(
                "param {} ({}) has a ParamInfo entry but get_plain returned None — \
                 derive macro inconsistency",
                pi.id, pi.name
            )
        });
        assert!(
            (current - pi.default_plain).abs() < 0.0001,
            "Param {} ({}) default mismatch: declared={}, actual={}",
            pi.id,
            pi.name,
            pi.default_plain,
            current
        );
    }
}

/// Assert normalized param values are clamped to [0, 1].
///
/// `set_plain` stores raw atomics (no clamping) but normalized
/// values should always round-trip within [0, 1].
///
/// # Panics
///
/// Panics if `get_normalized` returns `None` for an id that has a
/// `ParamInfo` entry, or if the read-back value escapes
/// `[-1e-4, 1+1e-4]` after writing 2.0 / -1.0.
pub fn assert_param_normalized_clamped<P: PluginExport>() {
    let plugin = P::create();
    let infos = plugin.params().param_infos();
    for pi in &infos {
        // Set above 1.0
        plugin.params().set_normalized(pi.id, 2.0);
        let val = plugin.params().get_normalized(pi.id).unwrap_or_else(|| {
            panic!(
                "param {} ({}) get_normalized returned None despite ParamInfo \
                 entry — derive macro inconsistency",
                pi.id, pi.name
            )
        });
        assert!(
            val <= 1.0001,
            "Param {} ({}) normalized not clamped above 1.0: set 2.0, got {}",
            pi.id,
            pi.name,
            val
        );

        // Set below 0.0
        plugin.params().set_normalized(pi.id, -1.0);
        let val = plugin.params().get_normalized(pi.id).unwrap_or_else(|| {
            panic!(
                "param {} ({}) get_normalized returned None despite ParamInfo \
                 entry — derive macro inconsistency",
                pi.id, pi.name
            )
        });
        assert!(
            val >= -0.0001,
            "Param {} ({}) normalized not clamped below 0.0: set -1.0, got {}",
            pi.id,
            pi.name,
            val
        );

        // Restore default
        plugin.params().set_plain(pi.id, pi.default_plain);
    }
}

/// Assert `set_normalized` → `get_normalized` round-trips for all params.
///
/// For discrete/bool/enum params, only tests boundary values (0.0, 1.0)
/// since intermediate values snap to the nearest discrete step.
///
/// # Panics
///
/// Panics if `get_normalized` returns `None` for an id with a
/// `ParamInfo` entry, or if the round-trip error exceeds the
/// per-param tolerance (half a step for discrete params, `1e-6` for
/// continuous).
pub fn assert_param_normalized_roundtrip<P: PluginExport>() {
    let plugin = P::create();
    let infos = plugin.params().param_infos();
    for pi in &infos {
        let (test_values, tolerance) = if let Some(steps) = pi.range.step_count() {
            // Discrete param: test exact step positions. Tolerance
            // sized for one-step quantization (half a step).
            let steps = steps.get();
            let v: Vec<f64> = (0..=steps).map(|i| f64::from(i) / f64::from(steps)).collect();
            (v, (0.5 / f64::from(steps)).max(1e-6))
        } else {
            // Continuous param: tighter tolerance — round-trip should
            // be exact modulo `clamp(0, 1)` and float rounding.
            (vec![0.0, 0.25, 0.5, 0.75, 1.0], 1e-6)
        };
        for &norm in &test_values {
            plugin.params().set_normalized(pi.id, norm);
            let got = plugin.params().get_normalized(pi.id).unwrap_or_else(|| {
                panic!(
                    "param {} ({}) get_normalized returned None despite ParamInfo \
                     entry — derive macro inconsistency",
                    pi.id, pi.name
                )
            });
            assert!(
                (got - norm).abs() <= tolerance,
                "Param {} ({}) normalized round-trip: set {norm}, got {got} (tol {tolerance})",
                pi.id,
                pi.name
            );
        }
        // Restore default
        plugin.params().set_plain(pi.id, pi.default_plain);
    }
}

/// Assert param count matches `param_infos` length.
///
/// # Panics
///
/// Panics if `count()` disagrees with `param_infos().len()`.
pub fn assert_param_count_matches<P: PluginExport>() {
    let plugin = P::create();
    let count = plugin.params().count();
    let infos = plugin.params().param_infos();
    assert_eq!(
        count,
        infos.len(),
        "param count() = {count}, but param_infos().len() = {}",
        infos.len()
    );
}

/// Assert all parameter IDs are unique.
///
/// # Panics
///
/// Panics on the first duplicate `id` encountered while iterating
/// `param_infos`.
pub fn assert_no_duplicate_param_ids<P: PluginExport>() {
    let plugin = P::create();
    let infos = plugin.params().param_infos();
    let mut seen = std::collections::HashSet::new();
    for pi in &infos {
        assert!(
            seen.insert(pi.id),
            "Duplicate parameter ID {}: {} (already used by another param)",
            pi.id,
            pi.name
        );
    }
}

// ---------------------------------------------------------------------------
// State resilience tests
// ---------------------------------------------------------------------------

/// Assert corrupt state data doesn't crash.
///
/// Each blob in the corpus must either deserialize cleanly OR return
/// `None` — and `restore_values` on a successful parse must not panic.
/// The previous form passed trivially when `deserialize_state` returned
/// `None` for everything (which would happen if the implementation
/// regressed to "always reject"), so we now also exercise at least one
/// valid blob to prove the code path under test is reachable.
///
/// # Panics
///
/// Panics if `deserialize_state` rejects a blob produced by
/// `snapshot_plugin` (sanity check — without this the test passes
/// trivially when `deserialize_state` is hard-broken), or if any of
/// the corruption probes (`deserialize_state` / `restore_values`)
/// itself panics.
pub fn assert_corrupt_state_no_crash<P: PluginExport>() {
    let info = P::info();
    let hash = state::hash_plugin_id(info.clap_id);

    let garbage: Vec<Vec<u8>> = vec![
        vec![0xFF; 64],                     // random bytes
        b"OAST".to_vec(),                   // valid magic, truncated
        vec![0; 4096],                      // all zeros
        vec![0xFF, 0xFE, 0xFD, 0xFC, 0xFB], // short garbage
    ];

    let plugin = P::create();
    for blob in &garbage {
        let result = state::deserialize_state(blob, hash);
        // Should return None (not panic)
        if let Some(d) = result {
            // Even if it parses, loading shouldn't crash
            plugin.params().restore_values(&d.params);
        }
    }

    // Sanity check: a freshly-snapshotted state for *this* plugin must
    // round-trip. Without this, the loop above would silently pass
    // even if `deserialize_state` was hard-broken (always-`None`).
    let mut snapshot_plugin = P::create();
    snapshot_plugin.init();
    let blob = state::snapshot_plugin(&snapshot_plugin);
    assert!(
        state::deserialize_state(&blob, hash).is_some(),
        "deserialize_state rejected a blob produced by snapshot_plugin — \
         the corruption test would pass trivially under this regression"
    );
}

/// Assert empty state data doesn't crash.
///
/// # Panics
///
/// Panics if `deserialize_state` returns `Some` for a zero-byte or
/// single-byte input (both must be rejected).
pub fn assert_empty_state_no_crash<P: PluginExport>() {
    let info = P::info();
    let hash = state::hash_plugin_id(info.clap_id);

    let result = state::deserialize_state(&[], hash);
    assert!(result.is_none(), "Empty state should return None");

    let result = state::deserialize_state(&[0], hash);
    assert!(result.is_none(), "Single-byte state should return None");
}

// ---------------------------------------------------------------------------
// GUI screenshot tests
// ---------------------------------------------------------------------------

// Render + save are in `truce-core` so non-test contexts (like
// `cargo truce` tooling) can invoke them without pulling in dev-deps.
pub use truce_core::screenshot::save_png;

// ---------------------------------------------------------------------------
// ScreenshotTest builder
// ---------------------------------------------------------------------------

use std::path::PathBuf;

/// Boxed closure handed to [`ScreenshotTest::setup`]. Aliased so the
/// `setup` field type stays readable instead of tripping clippy's
/// `type_complexity` lint.
type SetupFn<P> = Box<dyn FnOnce(&mut P)>;

/// Builder for a screenshot regression test.
///
/// Construct via the [`screenshot!`] macro:
/// `screenshot!(Plugin, "screenshots/main.png")`. The path is the
/// committed reference PNG location — relative to the calling
/// crate's `Cargo.toml` directory, or absolute. There's no implicit
/// directory and no auto-derived filename; every test names its
/// own reference.
///
/// Lifecycle: `P::create()` → `init()` → optional `state_file` load
/// → optional `set_param` shortcuts → optional `setup` closure →
/// render. Mirrors [`PluginDriver`]'s ordering so the same builder
/// vocabulary works for both audio and GUI tests.
///
/// # Examples
///
/// ```ignore
/// #[test]
/// fn screenshot() {
///     truce_test::screenshot!(Plugin, "screenshots/default.png").run();
/// }
///
/// // State-dependent: tweak params before rendering.
/// #[test]
/// fn screenshot_max_gain() {
///     truce_test::screenshot!(Plugin, "screenshots/max_gain.png")
///         .set_param(MyParamId::Gain, 1.0)
///         .run();
/// }
///
/// // Pre-saved state from the standalone host's Cmd+S.
/// #[test]
/// fn screenshot_evening() {
///     truce_test::screenshot!(Plugin, "screenshots/evening.png")
///         .state_file("test_states/evening.pluginstate")
///         .run();
/// }
/// ```
pub struct ScreenshotTest<P: PluginExport> {
    /// Reference PNG path, resolved at `new`-time. Absolute, or
    /// joined to `CARGO_MANIFEST_DIR` if the caller passed a
    /// relative path.
    ref_path: PathBuf,
    /// Manifest dir of the calling crate. Used to resolve the
    /// `state_file` path; not used after `ref_path` is built.
    manifest_dir: PathBuf,
    /// Max allowed differing-pixel count. `0` = strict.
    tolerance: usize,
    /// Per-pixel "different enough to count" threshold: a pixel only
    /// adds to `tolerance` if any RGBA channel differs from the
    /// reference by more than this. `0` = strict (any byte
    /// difference counts).
    pixel_threshold: u8,
    /// `.pluginstate` bytes loaded after init, before `set_param`
    /// shortcuts and `setup` closure.
    state_bytes: Option<Vec<u8>>,
    /// `.set_param(id, v)` shortcuts — applied after state load,
    /// before the `setup` closure.
    param_overrides: Vec<(u32, f64)>,
    /// Optional plugin mutation between `P::create()` and render.
    setup: Option<SetupFn<P>>,
}

impl<P: PluginExport> ScreenshotTest<P> {
    /// Internal constructor used by [`screenshot!`]. Plugin authors
    /// should not call this directly — the macro fills
    /// `manifest_dir` from the calling crate's compile-time
    /// `CARGO_MANIFEST_DIR`.
    #[doc(hidden)]
    pub fn __new(manifest_dir: &str, ref_path: impl Into<PathBuf>) -> Self {
        let manifest_dir = PathBuf::from(manifest_dir);
        let raw = ref_path.into();
        let ref_path = if raw.is_absolute() {
            raw
        } else {
            manifest_dir.join(raw)
        };
        Self {
            ref_path,
            manifest_dir,
            tolerance: 0,
            pixel_threshold: 0,
            state_bytes: None,
            param_overrides: Vec::new(),
            setup: None,
        }
    }

    /// Mutate the plugin between `P::create()` / `init()` and the
    /// render. Use this to set custom (non-param) state, drive a
    /// `process()` block to populate meters, etc.
    ///
    /// Composes with [`Self::state_file`] (state loads first) and
    /// [`Self::set_param`] (shortcuts apply first); the closure runs
    /// last.
    pub fn setup<F: FnOnce(&mut P) + 'static>(mut self, f: F) -> Self {
        self.setup = Some(Box::new(f));
        self
    }

    /// Set a parameter to a normalized [0, 1] value before the
    /// render. Equivalent to a `setup(|p| p.params().set_normalized(id, v))`
    /// closure but written as one builder call. Multiple `.set_param`
    /// calls compose; they apply after `.state_file` (if any) and
    /// before `.setup`.
    pub fn set_param(mut self, id: impl Into<u32>, normalized: f64) -> Self {
        self.param_overrides.push((id.into(), normalized));
        self
    }

    /// Read a `.pluginstate` file (the standalone host's `Cmd+S`
    /// save format) and apply it via `plugin.load_state(&bytes)`
    /// after init and before any `set_param` overrides / `setup`
    /// closure. Path is resolved relative to the crate's manifest
    /// dir, or used as-is if absolute.
    ///
    /// # Panics
    ///
    /// Panics if the file cannot be read (missing path, permission
    /// error, etc.) — the test failure points at the resolved path so
    /// it's easy to fix the call site.
    pub fn state_file<S: Into<PathBuf>>(mut self, path: S) -> Self {
        let raw = path.into();
        let resolved = if raw.is_absolute() {
            raw
        } else {
            self.manifest_dir.join(&raw)
        };
        let bytes = std::fs::read(&resolved)
            .unwrap_or_else(|e| panic!("state_file: failed to read {}: {e}", resolved.display()));
        self.state_bytes = Some(bytes);
        self
    }

    /// Max allowed differing-pixel count. `0` is strict equality;
    /// bump for cross-machine antialiasing tolerance.
    ///
    /// Composes with [`Self::pixel_threshold`]: a pixel only counts
    /// toward this budget if its max channel delta exceeds the
    /// threshold, so sub-perceptual AA wobble doesn't have to inflate
    /// `tolerance` to numbers that would also hide real regressions.
    #[must_use] 
    pub fn tolerance(mut self, t: usize) -> Self {
        self.tolerance = t;
        self
    }

    /// Per-pixel "different enough to count" threshold. A pixel
    /// only adds to the [`Self::tolerance`] budget if at least one
    /// of its R/G/B/A channels differs from the reference by more
    /// than this. `0` = strict (any byte difference counts).
    ///
    /// Practical values: `1`–`3` ignore tiny rasterizer / filter
    /// drift between machines without masking real visual changes;
    /// `8`+ starts to hide things a human would notice.
    #[must_use] 
    pub fn pixel_threshold(mut self, d: u8) -> Self {
        self.pixel_threshold = d;
        self
    }

    /// Build the plugin (with `state_file`/`set_param`/`setup`
    /// applied if present, in that order), render, and compare
    /// against the reference at the supplied path:
    ///
    /// - No reference → panic, pointing at
    ///   `cargo truce screenshot --out <ref_path>` to create one.
    /// - Match within tolerance → pass silently.
    /// - Mismatch → panic with both PNG paths and the `cp` command
    ///   to accept the new render as the baseline.
    pub fn run(self) {
        let ref_path = self.ref_path;
        let tolerance = self.tolerance;
        let pixel_threshold = self.pixel_threshold;
        let state_bytes = self.state_bytes;
        let param_overrides = self.param_overrides;
        let setup = self.setup;

        let mut plugin = P::create();
        plugin.init();
        if let Some(bytes) = state_bytes.as_deref() {
            plugin.load_state(bytes);
        }
        for (id, value) in &param_overrides {
            plugin.params().set_normalized(*id, *value);
        }
        plugin.params().snap_smoothers();
        if let Some(f) = setup {
            f(&mut plugin);
        }
        let (pixels, w, h) = truce_core::screenshot::render_pixels_for::<P>(&mut plugin);
        compare_against_reference(
            &pixels,
            w,
            h,
            &ref_path,
            tolerance,
            pixel_threshold,
            Some(&self.manifest_dir),
        );
    }
}

/// Construct a [`ScreenshotTest`] for the given plugin type, with
/// the reference-PNG path required as the second argument. The
/// path is anchored to the calling crate's `CARGO_MANIFEST_DIR`
/// when relative, or used as-is when absolute.
///
/// ```ignore
/// #[test]
/// fn screenshot() {
///     truce_test::screenshot!(Plugin, "screenshots/default.png").run();
/// }
/// ```
#[macro_export]
macro_rules! screenshot {
    ($plugin:ty, $path:expr $(,)?) => {
        $crate::ScreenshotTest::<$plugin>::__new(env!("CARGO_MANIFEST_DIR"), $path)
    };
}

/// Compare RGBA pixels against the reference PNG at `ref_path`.
/// Render gets saved to `<workspace>/target/screenshots/<basename>`
/// regardless of where the reference lives, so a failed comparison
/// always has a sibling artifact to inspect.
///
/// `manifest_dir_hint`, when given, is the calling crate's
/// `CARGO_MANIFEST_DIR` (captured at compile time by the
/// `screenshot!` macro). Walking up from there to the workspace root
/// is more reliable than walking up from CWD — the latter is
/// mis-anchored when tests run from a different directory or when
/// CWD is inside `target/`.
fn compare_against_reference(
    pixels: &[u8],
    width: u32,
    height: u32,
    ref_path: &std::path::Path,
    max_diff_pixels: usize,
    pixel_threshold: u8,
    manifest_dir_hint: Option<&std::path::Path>,
) {
    let render_dir = workspace_target_screenshots_dir(manifest_dir_hint);
    let render_path = render_dir.join(
        ref_path
            .file_name()
            .map(std::path::Path::new).map_or_else(|| PathBuf::from("screenshot.png"), std::path::Path::to_path_buf),
    );

    if !ref_path.exists() {
        // No baseline — save the current render so the user can
        // inspect it before committing.
        std::fs::create_dir_all(&render_dir).ok();
        save_png(&render_path, pixels, width, height);
        panic!(
            "No screenshot baseline at {ref}. Just-rendered PNG saved at {rendered}.\n\
             Create the baseline with: cargo truce screenshot --out {ref}\n\
             then inspect the rendered PNG and commit it.",
            ref = ref_path.display(),
            rendered = render_path.display(),
        );
    }

    let (ref_pixels, ref_w, ref_h) = truce_core::screenshot::load_png(ref_path);
    if (width, height) != (ref_w, ref_h) {
        std::fs::create_dir_all(&render_dir).ok();
        save_png(&render_path, pixels, width, height);
        panic!(
            "GUI size changed: current {width}x{height}, reference {ref_w}x{ref_h}. \
             Just-rendered PNG saved at {rendered}.\n\
             Regenerate the baseline with: cargo truce screenshot --out {ref}\n\
             then inspect the rendered PNG and commit it.",
            rendered = render_path.display(),
            ref = ref_path.display(),
        );
    }

    // Walk pixel-by-pixel (4 bytes each), counting only pixels whose
    // max RGBA channel delta exceeds `pixel_threshold`. Threshold = 0
    // recovers strict byte-equality at pixel granularity.
    let mut diff_count = 0usize;
    let mut max_delta_seen: u8 = 0;
    for (cur, refp) in pixels.chunks_exact(4).zip(ref_pixels.chunks_exact(4)) {
        let delta = cur
            .iter()
            .zip(refp.iter())
            .map(|(c, r)| c.abs_diff(*r))
            .max()
            .unwrap_or(0);
        if delta > pixel_threshold {
            diff_count += 1;
        }
        if delta > max_delta_seen {
            max_delta_seen = delta;
        }
    }

    if diff_count > max_diff_pixels {
        // Save the failing render only on failure — successful tests
        // no longer eat I/O writing artifacts they don't need.
        std::fs::create_dir_all(&render_dir).ok();
        save_png(&render_path, pixels, width, height);
        panic!(
            "GUI screenshot mismatch: {diff_count} pixels differ above threshold {pixel_threshold} \
             (max allowed: {max_diff_pixels}; largest channel delta seen: {max_delta_seen}).\n\
             Reference: {}\n\
             Current:   {}\n\
             Either fix the regression, or accept the new render with: cp '{}' '{}'",
            ref_path.display(),
            render_path.display(),
            render_path.display(),
            ref_path.display(),
        );
    }
}

/// `<cargo-target-dir>/screenshots/`. Walks up from CWD looking for
/// the topmost `Cargo.toml` (preferring one with `[workspace]`) to
/// anchor the resolution, then routes through `truce_build::target_dir`
/// so `CARGO_TARGET_DIR` and `<root>/.cargo/config.toml`'s
/// `[build].target-dir` both override the literal `target/`. Used
/// only for the failing-render artifact path — committed reference
/// paths come from the builder's manifest-dir-anchored resolution.
fn workspace_target_screenshots_dir(manifest_dir_hint: Option<&std::path::Path>) -> PathBuf {
    // Prefer the calling crate's `CARGO_MANIFEST_DIR` (captured at
    // compile time and threaded through the `screenshot!` macro). It's
    // a stable anchor regardless of where `cargo test` runs from. Fall
    // back to CWD only when no hint is available — old code paths or
    // direct calls into this function.
    let start = manifest_dir_hint.map_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")), std::path::Path::to_path_buf);
    let mut dir = start.clone();
    let mut topmost_package: Option<PathBuf> = None;
    loop {
        let toml_path = dir.join("Cargo.toml");
        if toml_path.exists()
            && let Ok(s) = std::fs::read_to_string(&toml_path)
            && let Ok(doc) = s.parse::<toml::Table>()
        {
            // Workspace `Cargo.toml` is the strongest anchor we'll
            // see — short-circuit and take its enclosing dir as
            // the target-dir root.
            if doc.contains_key("workspace") {
                return truce_build::target_dir(&dir).join("screenshots");
            }
            // Otherwise we may be under a single-crate or workspace
            // member. Remember the topmost package and keep walking
            // — if we never find a workspace, the topmost package
            // is the right anchor.
            if doc.contains_key("package") {
                topmost_package = Some(dir.clone());
            }
        }
        if !dir.pop() {
            let anchor = topmost_package.unwrap_or(start);
            return truce_build::target_dir(&anchor).join("screenshots");
        }
    }
}
