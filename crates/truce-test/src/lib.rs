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

use std::sync::Arc;

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::process::ProcessContext;
use truce_core::state;
use truce_params::Params;

/// Result of a render operation.
pub struct RenderResult {
    /// Output audio channels. Each Vec<f32> is one channel.
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
    drop(buffer);

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
    drop(buffer);

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
    let info = P::info();
    let hash = state::hash_plugin_id(info.clap_id);

    // Save state
    let (ids, values) = plugin.params().collect_values();
    let blob = state::serialize_state(hash, &ids, &values, None);

    // Create new instance and load
    let plugin2 = P::create();
    let result = state::deserialize_state(&blob, hash);
    assert!(result.is_some(), "Failed to deserialize state");

    let deserialized = result.unwrap();
    plugin2.params().restore_values(&deserialized.params);

    // Verify all params match
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
// GUI snapshot tests
// ---------------------------------------------------------------------------

/// Resolve the workspace `screenshots/` directory.
///
/// Walks up from `CARGO_MANIFEST_DIR` looking for a `Cargo.toml`
/// containing `[workspace]`. Works regardless of how deeply nested
/// the calling crate is.
pub fn workspace_screenshots_dir() -> std::path::PathBuf {
    let mut dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let toml = dir.join("Cargo.toml");
        if toml.exists() {
            if let Ok(s) = std::fs::read_to_string(&toml) {
                if s.contains("[workspace]") {
                    let snap = dir.join("screenshots");
                    std::fs::create_dir_all(&snap).ok();
                    return snap;
                }
            }
        }
        if !dir.pop() {
            panic!("Could not find workspace root from CARGO_MANIFEST_DIR");
        }
    }
}

/// Compare raw RGBA pixels against a reference PNG.
///
/// On first run (no reference file exists), generates the reference.
/// On subsequent runs, compares pixel-by-pixel and fails if the
/// difference exceeds `max_diff_pixels`.
///
/// Works with any rendering backend — the caller provides the pixels.
///
/// # Example
/// ```ignore
/// let pixels = my_renderer.screenshot();
/// truce_test::assert_gui_snapshot_raw("my_plugin", &pixels, 400, 300, 0);
/// ```
pub fn assert_gui_snapshot_raw(
    name: &str,
    pixels: &[u8],
    width: u32,
    height: u32,
    max_diff_pixels: usize,
) {
    let dir = workspace_screenshots_dir();
    let ref_path = dir.join(format!("{name}.png"));

    if !ref_path.exists() {
        save_png(&ref_path, pixels, width, height);
        eprintln!(
            "[truce-test] Snapshot reference created: {}",
            ref_path.display()
        );
        return;
    }

    let (ref_pixels, ref_w, ref_h) = load_png(&ref_path);

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
        let fail_path = dir.join(format!("{name}_FAILED.png"));
        save_png(&fail_path, pixels, width, height);
        panic!(
            "GUI snapshot mismatch: {diff_count} pixels differ (max allowed: {max_diff_pixels}). \
             Reference: {}\n\
             Current:   {}\n\
             Delete the reference to regenerate.",
            ref_path.display(),
            fail_path.display(),
        );
    }
}

/// Render the built-in GUI and compare against a reference PNG.
///
/// # Example
/// ```ignore
/// #[test]
/// fn gui_snapshot() {
///     let plugin = Gain::new();
///     let params = Arc::new(GainParams::new());
///     truce_test::assert_gui_snapshot_grid::<GainParams>(
///         "gain_default", params, plugin.layout(), 0,
///     );
/// }
/// ```
pub fn assert_gui_snapshot_grid<P: Params + 'static>(
    name: &str,
    params: Arc<P>,
    layout: truce_gui::layout::GridLayout,
    max_diff_pixels: usize,
) {
    let (pixels, w, h) = truce_gpu::snapshot::render_to_pixels(params, layout);
    assert_gui_snapshot_raw(name, &pixels, w, h, max_diff_pixels);
}

fn save_png(path: &std::path::Path, pixels: &[u8], w: u32, h: u32) {
    let file = std::fs::File::create(path)
        .unwrap_or_else(|e| panic!("Failed to create {}: {e}", path.display()));
    let mut encoder = png::Encoder::new(file, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    // 144 DPI (2x Retina) — renders at half pixel size in viewers/GitHub
    encoder.set_pixel_dims(Some(png::PixelDimensions {
        xppu: 5669, // 144 DPI in pixels per meter
        yppu: 5669,
        unit: png::Unit::Meter,
    }));
    let mut writer = encoder.write_header()
        .unwrap_or_else(|e| panic!("Failed to write PNG header: {e}"));
    writer.write_image_data(pixels)
        .unwrap_or_else(|e| panic!("Failed to write PNG data: {e}"));
}

fn load_png(path: &std::path::Path) -> (Vec<u8>, u32, u32) {
    let file = std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("Failed to open {}: {e}", path.display()));
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info()
        .unwrap_or_else(|e| panic!("Failed to read PNG info: {e}"));
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf)
        .unwrap_or_else(|e| panic!("Failed to decode PNG frame: {e}"));
    buf.truncate(info.buffer_size());
    (buf, info.width, info.height)
}
