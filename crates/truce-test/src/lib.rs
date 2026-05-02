//! Test utilities for truce plugins.
//!
//! Provides helpers to render audio, inject MIDI, verify output,
//! and round-trip state — all in-process, no host simulation needed.
//!
//! # Usage
//!
//! Add to your plugin crate's `[dev-dependencies]`:
//! ```toml
//! [dev-dependencies]
//! truce-test = { workspace = true }
//! ```
//!
//! Then in your tests:
//! ```ignore
//! #[test]
//! fn effect_produces_audio() {
//!     let result = truce_test::render_effect::<MyEffect>(512, 44100.0);
//!     truce_test::assert_nonzero(&result.output);
//! }
//! ```

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::process::ProcessContext;
use truce_core::state;
use truce_params::Params;

/// In-process plugin runner + time-windowed / meter / clipping
/// assertion helpers, built on [`truce_standalone::in_process`].
/// Behind the `in-process` feature so this crate's default use —
/// render / state / params / GUI assertions — doesn't pull cpal /
/// midir transitively.
#[cfg(feature = "in-process")]
pub mod in_process;

/// Result of a render operation.
pub struct RenderResult {
    /// Output audio channels. Each `Vec<f32>` is one channel.
    pub output: Vec<Vec<f32>>,
    /// The plugin instance (for further inspection).
    pub num_frames: usize,
}

/// Render N frames through an effect plugin.
///
/// Creates a new instance, resets at the given sample rate,
/// fills input with 0.5 on all channels, and processes.
pub fn render_effect<P: PluginExport>(frames: usize, sample_rate: f64) -> RenderResult {
    let mut plugin = P::create();
    plugin.init();
    plugin.reset(sample_rate, frames);
    plugin.params().set_sample_rate(sample_rate);
    plugin.params().snap_smoothers();

    let _info = P::info();
    let layouts = P::bus_layouts();
    let layout = &layouts[0];
    let num_in = layout.total_input_channels() as usize;
    let num_out = layout.total_output_channels() as usize;

    // Create input buffers filled with 0.5
    let input_data: Vec<Vec<f32>> = (0..num_in).map(|_| vec![0.5f32; frames]).collect();
    let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();

    // Create output buffers
    let mut output_data: Vec<Vec<f32>> = (0..num_out).map(|_| vec![0.0f32; frames]).collect();

    // Copy input to output (in-place processing, like hosts do for effects)
    for ch in 0..num_in.min(num_out) {
        output_data[ch].copy_from_slice(&input_data[ch]);
    }

    let mut output_slices: Vec<&mut [f32]> =
        output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

    let mut buffer = unsafe { AudioBuffer::from_slices(&input_slices, &mut output_slices, frames) };
    let events = EventList::new();
    let transport = TransportInfo::default();
    let mut output_events = EventList::new();
    let mut context = ProcessContext::new(&transport, sample_rate, frames, &mut output_events);

    plugin.process(&mut buffer, &events, &mut context);
    _ = buffer;

    let output: Vec<Vec<f32>> = output_data;
    RenderResult {
        output,
        num_frames: frames,
    }
}

/// Render N frames through an instrument plugin with MIDI events.
///
/// Creates a new instance, resets, injects the given events, and processes.
/// No audio input (instruments generate output from MIDI).
pub fn render_instrument<P: PluginExport>(
    frames: usize,
    sample_rate: f64,
    midi_events: &[Event],
) -> RenderResult {
    let mut plugin = P::create();
    plugin.init();
    plugin.reset(sample_rate, frames);
    plugin.params().set_sample_rate(sample_rate);
    plugin.params().snap_smoothers();

    let layouts = P::bus_layouts();
    let layout = &layouts[0];
    let num_out = layout.total_output_channels() as usize;

    let input_slices: Vec<&[f32]> = vec![];
    let mut output_data: Vec<Vec<f32>> = (0..num_out).map(|_| vec![0.0f32; frames]).collect();
    let mut output_slices: Vec<&mut [f32]> =
        output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

    let mut buffer = unsafe { AudioBuffer::from_slices(&input_slices, &mut output_slices, frames) };
    let mut events = EventList::new();
    for ev in midi_events {
        events.push(ev.clone());
    }
    let transport = TransportInfo::default();
    let mut output_events = EventList::new();
    let mut context = ProcessContext::new(&transport, sample_rate, frames, &mut output_events);

    plugin.process(&mut buffer, &events, &mut context);
    _ = buffer;

    let output: Vec<Vec<f32>> = output_data;
    RenderResult {
        output,
        num_frames: frames,
    }
}

/// Create a Note On event.
pub fn note_on(note: u8, velocity: u8, offset: u32) -> Event {
    Event {
        sample_offset: offset,
        body: EventBody::NoteOn {
            channel: 0,
            note,
            velocity: velocity as f32 / 127.0,
        },
    }
}

/// Create a Note Off event.
pub fn note_off(note: u8, offset: u32) -> Event {
    Event {
        sample_offset: offset,
        body: EventBody::NoteOff {
            channel: 0,
            note,
            velocity: 0.0,
        },
    }
}

/// Assert that at least one channel has audio above the threshold.
pub fn assert_nonzero(output: &[Vec<f32>]) {
    let max = output
        .iter()
        .flat_map(|ch| ch.iter())
        .map(|s| s.abs())
        .fold(0.0f32, f32::max);
    assert!(
        max > 0.001,
        "Expected non-zero audio output, but max sample was {max}"
    );
}

/// Assert all channels are silence (below threshold).
pub fn assert_silence(output: &[Vec<f32>]) {
    let max = output
        .iter()
        .flat_map(|ch| ch.iter())
        .map(|s| s.abs())
        .fold(0.0f32, f32::max);
    assert!(max < 0.001, "Expected silence, but max sample was {max}");
}

/// Assert no NaN or Inf values in output.
pub fn assert_no_nans(output: &[Vec<f32>]) {
    for (ch, data) in output.iter().enumerate() {
        for (i, &s) in data.iter().enumerate() {
            assert!(s.is_finite(), "NaN/Inf at channel {ch} sample {i}: {s}");
        }
    }
}

/// Assert state save/load round-trips correctly.
///
/// Saves state, creates a new instance, loads state, and verifies
/// all parameter values match.
pub fn assert_state_round_trip<P: PluginExport>() {
    let plugin = P::create();
    let blob = state::snapshot_plugin(&plugin);

    let mut plugin2 = P::create();
    state::restore_plugin(&mut plugin2, &blob).expect("restore_plugin failed");

    let param_infos = plugin.params().param_infos();
    for pi in &param_infos {
        let v1 = plugin.params().get_plain(pi.id).unwrap();
        let v2 = plugin2.params().get_plain(pi.id).unwrap();
        assert!(
            (v1 - v2).abs() < 0.0001,
            "Param {} ({}) mismatch: {v1} vs {v2}",
            pi.id,
            pi.name
        );
    }
}

/// Assert the plugin has a working editor with valid dimensions.
pub fn assert_has_editor<P: PluginExport>() {
    let mut plugin = P::create();
    let editor = plugin.editor();
    assert!(editor.is_some(), "Plugin::editor() returned None");
    let editor = editor.unwrap();
    let (w, h) = editor.size();
    assert!(w > 0 && h > 0, "Editor size is zero: {w}x{h}");
}

/// Assert plugin_info!() returns valid metadata.
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
/// Catches the FourCharCode endianness bug (big-endian on ARM64).
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

/// Assert AU FourCharCode round-trips through big-endian u32.
///
/// This is the encoding used by AudioComponentDescription on macOS.
pub fn assert_fourcc_roundtrip<P: PluginExport>() {
    let info = P::info();
    for (label, code) in [
        ("au_type", info.au_type),
        ("fourcc", info.fourcc),
        ("au_manufacturer", info.au_manufacturer),
    ] {
        let packed = ((code[0] as u32) << 24)
            | ((code[1] as u32) << 16)
            | ((code[2] as u32) << 8)
            | (code[3] as u32);
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
/// Catches the GarageBand SupportedNumChannels bug — instruments must
/// report 0 input channels for AU hosts to show them.
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
pub fn assert_param_defaults_match<P: PluginExport>() {
    let plugin = P::create();
    let infos = plugin.params().param_infos();
    for pi in &infos {
        let current = plugin.params().get_plain(pi.id).unwrap();
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
/// set_plain stores raw atomics (no clamping) but normalized
/// values should always round-trip within [0, 1].
pub fn assert_param_normalized_clamped<P: PluginExport>() {
    let plugin = P::create();
    let infos = plugin.params().param_infos();
    for pi in &infos {
        // Set above 1.0
        plugin.params().set_normalized(pi.id, 2.0);
        let val = plugin.params().get_normalized(pi.id).unwrap();
        assert!(
            val <= 1.0001,
            "Param {} ({}) normalized not clamped above 1.0: set 2.0, got {}",
            pi.id,
            pi.name,
            val
        );

        // Set below 0.0
        plugin.params().set_normalized(pi.id, -1.0);
        let val = plugin.params().get_normalized(pi.id).unwrap();
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

/// Assert set_normalized → get_normalized round-trips for all params.
///
/// For discrete/bool/enum params, only tests boundary values (0.0, 1.0)
/// since intermediate values snap to the nearest discrete step.
pub fn assert_param_normalized_roundtrip<P: PluginExport>() {
    let plugin = P::create();
    let infos = plugin.params().param_infos();
    for pi in &infos {
        let steps = pi.range.step_count();
        let test_values: Vec<f64> = if steps > 0 {
            // Discrete param: test exact step positions
            (0..=steps).map(|i| i as f64 / steps as f64).collect()
        } else {
            // Continuous param: test arbitrary positions
            vec![0.0, 0.25, 0.5, 0.75, 1.0]
        };
        for &norm in &test_values {
            plugin.params().set_normalized(pi.id, norm);
            let got = plugin.params().get_normalized(pi.id).unwrap();
            assert!(
                (got - norm).abs() < 0.02,
                "Param {} ({}) normalized round-trip: set {norm}, got {got}",
                pi.id,
                pi.name
            );
        }
        // Restore default
        plugin.params().set_plain(pi.id, pi.default_plain);
    }
}

/// Assert param count matches param_infos length.
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
    for (i, blob) in garbage.iter().enumerate() {
        let result = state::deserialize_state(blob, hash);
        // Should return None (not panic)
        if let Some(d) = result {
            // Even if it parses, loading shouldn't crash
            plugin.params().restore_values(&d.params);
        }
        // If we get here without panic, the test passes
        let _ = i; // suppress unused
    }
}

/// Assert empty state data doesn't crash.
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

/// Builder for a screenshot regression test.
///
/// Construct via the [`screenshot!`] macro (which fills in the
/// calling crate's manifest dir + crate name from `env!`). Configure
/// with `setup` / `state_file` / `name` / `path` / `tolerance`,
/// then call `run()` inside a `#[test]` fn.
///
/// # Examples
///
/// ```ignore
/// // Simplest: render the editor with default params and compare
/// // against `<crate>/screenshots/<crate>.png`.
/// #[test]
/// fn screenshot() {
///     truce_test::screenshot!(Plugin).run();
/// }
///
/// // State-dependent: tweak params before rendering, compare against
/// // `<crate>/screenshots/max_gain.png`.
/// #[test]
/// fn screenshot_max_gain() {
///     truce_test::screenshot!(Plugin)
///         .name("max_gain")
///         .setup(|p| p.params().gain.set_normalized(1.0))
///         .run();
/// }
///
/// // Pre-saved state from the standalone host's Cmd+S.
/// #[test]
/// fn screenshot_evening() {
///     truce_test::screenshot!(Plugin)
///         .name("evening")
///         .state_file("test_states/evening.pluginstate")
///         .run();
/// }
/// ```
pub struct ScreenshotTest<P: PluginExport> {
    /// Crate's manifest dir, captured from the macro's `env!`.
    /// Anchors all relative paths.
    manifest_dir: PathBuf,
    /// Crate name, captured from the macro's `env!`. Default
    /// filename stem.
    crate_name: &'static str,
    /// Where the reference PNG lives. Default:
    /// `<manifest_dir>/screenshots/<crate>.png`.
    path: PathSpec,
    /// Max allowed differing-pixel count. `0` = strict.
    tolerance: usize,
    /// Optional plugin mutation between `P::create()` and render.
    setup: Option<Box<dyn FnOnce(&mut P)>>,
    _marker: std::marker::PhantomData<P>,
}

enum PathSpec {
    /// `<manifest_dir>/screenshots/<crate_name>.png`
    Default,
    /// `<manifest_dir>/screenshots/<name>.png`
    Named(String),
    /// Caller-supplied. Absolute, or relative to `manifest_dir`.
    Explicit(PathBuf),
}

impl<P: PluginExport> ScreenshotTest<P> {
    /// Internal constructor used by [`screenshot!`]. Plugin authors
    /// should not call this directly — the macro fills in
    /// `manifest_dir` / `crate_name` from the calling crate's
    /// compile-time `env!` values.
    #[doc(hidden)]
    pub fn __from_env(manifest_dir: &str, crate_name: &'static str) -> Self {
        Self {
            manifest_dir: PathBuf::from(manifest_dir),
            crate_name,
            path: PathSpec::Default,
            tolerance: 0,
            setup: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// Mutate the plugin between `P::create()` / `init()` and the
    /// render. Use this to set params, load a state blob, drive a
    /// `process()` block to populate meters, etc.
    pub fn setup<F: FnOnce(&mut P) + 'static>(mut self, f: F) -> Self {
        self.setup = Some(Box::new(f));
        self
    }

    /// Shorthand for `.setup(|p| p.load_state(&fs::read(path)?))`.
    /// Mirrors the CLI's `--state` flag — a `.pluginstate` file the
    /// standalone host wrote becomes the rendered state. `path` is
    /// resolved relative to the crate's manifest dir, or used as-is
    /// if absolute.
    pub fn state_file<S: Into<PathBuf>>(self, path: S) -> Self {
        let raw = path.into();
        let resolved = if raw.is_absolute() {
            raw
        } else {
            self.manifest_dir.join(&raw)
        };
        self.setup(move |p| {
            let bytes = std::fs::read(&resolved).unwrap_or_else(|e| {
                panic!("state_file: failed to read {}: {e}", resolved.display())
            });
            p.load_state(&bytes);
        })
    }

    /// Use the conventional `screenshots/` directory but a different
    /// filename stem. Useful for multiple screenshots per plugin
    /// (`"main"`, `"panel_open"`, …).
    pub fn name<S: Into<String>>(mut self, name: S) -> Self {
        self.path = PathSpec::Named(name.into());
        self
    }

    /// Explicit reference-PNG path. Absolute paths are used as-is;
    /// relative paths are resolved against the calling crate's
    /// manifest dir.
    pub fn path<S: Into<PathBuf>>(mut self, path: S) -> Self {
        self.path = PathSpec::Explicit(path.into());
        self
    }

    /// Max allowed differing-pixel count on the reference platform.
    /// `0` is strict equality; bump for cross-machine antialiasing
    /// tolerance.
    pub fn tolerance(mut self, t: usize) -> Self {
        self.tolerance = t;
        self
    }

    fn resolve_path(&self) -> PathBuf {
        match &self.path {
            PathSpec::Default => self
                .manifest_dir
                .join("screenshots")
                .join(format!("{}.png", self.crate_name)),
            PathSpec::Named(n) => self
                .manifest_dir
                .join("screenshots")
                .join(format!("{n}.png")),
            PathSpec::Explicit(p) => {
                if p.is_absolute() {
                    p.clone()
                } else {
                    self.manifest_dir.join(p)
                }
            }
        }
    }

    /// Build the plugin (with `setup` applied if present), render,
    /// and compare against the reference. Same comparator semantics
    /// as the lower-level `assert_screenshot_pixels`:
    ///
    /// - No reference → log a `cp` hint and pass.
    /// - Match within tolerance → pass silently.
    /// - Mismatch on reference platform → panic.
    /// - Mismatch on non-reference platform → log + pass.
    pub fn run(self) {
        let ref_path = self.resolve_path();
        let tolerance = self.tolerance;
        let setup = self.setup;
        let mut plugin = P::create();
        plugin.init();
        if let Some(f) = setup {
            f(&mut plugin);
        }
        let (pixels, w, h) = truce_core::screenshot::render_pixels_for::<P>(&mut plugin);
        compare_against_reference(&pixels, w, h, &ref_path, tolerance);
    }
}

/// Construct a [`ScreenshotTest`] for the given plugin type, anchored
/// to the calling crate's manifest dir + crate name.
///
/// ```ignore
/// #[test]
/// fn screenshot() {
///     truce_test::screenshot!(Plugin).run();
/// }
/// ```
#[macro_export]
macro_rules! screenshot {
    ($plugin:ty) => {
        $crate::ScreenshotTest::<$plugin>::__from_env(
            env!("CARGO_MANIFEST_DIR"),
            env!("CARGO_PKG_NAME"),
        )
    };
}

/// Compare RGBA pixels against the reference PNG at `ref_path`.
/// Render gets saved to `<workspace>/target/screenshots/<basename>`
/// regardless of where the reference lives, so a failed comparison
/// always has a sibling artifact to inspect.
fn compare_against_reference(
    pixels: &[u8],
    width: u32,
    height: u32,
    ref_path: &std::path::Path,
    max_diff_pixels: usize,
) {
    // Render artifact lives in `target/screenshots/` — gitignored,
    // colocated with whatever workspace owns the test invocation.
    let render_dir = workspace_target_screenshots_dir();
    std::fs::create_dir_all(&render_dir).ok();
    let render_path = render_dir.join(
        ref_path
            .file_name()
            .map(std::path::Path::new)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("screenshot.png")),
    );
    save_png(&render_path, pixels, width, height);

    if !ref_path.exists() {
        eprintln!(
            "[truce-test] No reference at {}.\n\
             Current render saved to {}.\n\
             To promote: cp '{}' '{}'",
            ref_path.display(),
            render_path.display(),
            render_path.display(),
            ref_path.display(),
        );
        return;
    }

    let (ref_pixels, ref_w, ref_h) = truce_core::screenshot::load_png(ref_path);
    assert_eq!(
        (width, height),
        (ref_w, ref_h),
        "GUI size changed: current {width}x{height}, reference {ref_w}x{ref_h}. \
         Delete {} to regenerate.",
        ref_path.display()
    );

    let mut diff_count = 0usize;
    for (&current, &reference) in pixels.iter().zip(ref_pixels.iter()) {
        if current != reference {
            diff_count += 1;
        }
    }

    if diff_count > max_diff_pixels {
        if truce_core::screenshot::is_reference_platform() {
            panic!(
                "GUI screenshot mismatch: {diff_count} pixels differ (max allowed: {max_diff_pixels}).\n\
                 Reference: {}\n\
                 Current:   {}\n\
                 Either fix the regression, or accept the new render with: cp '{}' '{}'",
                ref_path.display(),
                render_path.display(),
                render_path.display(),
                ref_path.display(),
            );
        } else {
            // Non-reference platform: report the diff for visibility
            // but don't fail. Per-backend wgpu rasterization
            // differences make divergence expected.
            eprintln!(
                "[truce-test] non-reference diff on {}: {diff_count} pixels differ vs {} \
                 (informational; max allowed on reference: {max_diff_pixels}). \
                 Current render at {}.",
                std::env::consts::OS,
                ref_path.display(),
                render_path.display(),
            );
        }
    }
}

/// `<workspace_or_package_root>/target/screenshots/`. Walks up from
/// CWD looking for the topmost `Cargo.toml` (preferring one with
/// `[workspace]`). Used only for the failing-render artifact path —
/// committed reference paths come from the builder's
/// manifest-dir-anchored resolution.
fn workspace_target_screenshots_dir() -> PathBuf {
    let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = start.clone();
    let mut topmost_package: Option<PathBuf> = None;
    loop {
        let toml = dir.join("Cargo.toml");
        if toml.exists()
            && let Ok(s) = std::fs::read_to_string(&toml)
        {
            if s.contains("[workspace]") {
                return dir.join("target/screenshots");
            }
            topmost_package = Some(dir.clone());
        }
        if !dir.pop() {
            return topmost_package.unwrap_or(start).join("target/screenshots");
        }
    }
}

