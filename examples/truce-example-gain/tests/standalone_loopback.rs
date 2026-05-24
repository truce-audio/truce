//! Real-time loopback smoke test for the gain standalone.
//!
//! Runs the `truce-example-gain-standalone` binary in real-time
//! render mode (`--input-file` + `--output-file`, *without*
//! `--no-playback`) so the cpal audio host is actually opened. This
//! is the path that hit the Linux/ALSA "Default Audio Device" name-
//! resolution bug — the offline `--no-playback` path bypasses cpal
//! entirely and would not catch a regression there.
//!
//! Two variants:
//!   1. unity gain (default)            → output RMS ≈ input RMS
//!   2. gain = -6.02 dB via `--state`   → output RMS ≈ 0.5 × input RMS
//!
//! Needs a working default audio output. Linux CI sets the ALSA
//! default PCM to `null`; macOS runners and most dev machines
//! satisfy this via stock `CoreAudio` / `PulseAudio` / `PipeWire`.
//! Skipped on Windows: GH-hosted Windows runners ship without an
//! audio endpoint, and the canonical virtual-driver install (Scream
//! via `pnputil`) hangs for tens of minutes on those runners. The
//! bug this test guards against is Linux/ALSA-specific anyway, so
//! Linux + macOS coverage is enough.
//!
//! The `standalone-playback` feature is on by default for this
//! example, so `cargo build --workspace --all-targets` builds the
//! bin and the test finds it under `target/<profile>/`.

#![cfg(not(target_os = "windows"))]

use std::env;
use std::f32::consts::PI;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use truce_core::plugin::PluginRuntime as _;
use truce_core::state::{serialize_state, shared_plugin_state_hash};
use truce_example_gain::{GainParamsParamId, Plugin};

const SAMPLE_RATE: u32 = 48_000;
const DURATION_SECS: f32 = 1.0;
const SINE_FREQ_HZ: f32 = 440.0;
const SINE_AMPLITUDE: f32 = 0.5;

/// Tolerance on RMS ratios. Real-time render can drop the leading
/// block (cpal warm-up) and trailing block (EOF grace), so allow a
/// small slack. 5% is well below the 50% headroom between the unity
/// and -6 dB cases — either outcome lands far from the other side.
const RMS_TOLERANCE: f32 = 0.05;

#[test]
fn unity_gain_passthrough() {
    let bin = locate_standalone_bin();
    let tmp = make_tmpdir("truce-loopback-unity");
    let input_path = tmp.join("sine.wav");
    let output_path = tmp.join("out.wav");
    write_sine_wav(&input_path);

    run_standalone(&bin, &input_path, &output_path, None);

    let input = read_wav_mono(&input_path);
    let output = read_wav_mono(&output_path);
    let in_rms = rms(&input);
    let out_rms = active_rms(&output, in_rms * 0.1);
    assert_ratio(out_rms / in_rms, 1.0, RMS_TOLERANCE, "unity gain");
}

#[test]
fn minus_six_db_halves_amplitude() {
    let bin = locate_standalone_bin();
    let tmp = make_tmpdir("truce-loopback-half");
    let input_path = tmp.join("sine.wav");
    let output_path = tmp.join("out.wav");
    let state_path = tmp.join("gain.pluginstate");
    write_sine_wav(&input_path);
    // -6.0206 dB is exactly 0.5x amplitude (20·log10(0.5)).
    write_gain_state(&state_path, -6.0206);

    run_standalone(&bin, &input_path, &output_path, Some(&state_path));

    let input = read_wav_mono(&input_path);
    let output = read_wav_mono(&output_path);
    let in_rms = rms(&input);
    let out_rms = active_rms(&output, in_rms * 0.05);
    assert_ratio(out_rms / in_rms, 0.5, RMS_TOLERANCE, "-6 dB gain");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn locate_standalone_bin() -> PathBuf {
    let target_root = workspace_target_dir();
    let exe = format!("truce-example-gain-standalone{}", env::consts::EXE_SUFFIX);
    for profile in ["release", "debug"] {
        let candidate = target_root.join(profile).join(&exe);
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "{exe} not found under {}. The `standalone-playback` feature \
         on truce-example-gain is enabled by default - did \
         `cargo build` run for this workspace?",
        target_root.display(),
    );
}

fn workspace_target_dir() -> PathBuf {
    // Tests run with CARGO_MANIFEST_DIR = .../examples/truce-example-gain.
    // Workspace root is two levels up; target/ lives there unless
    // CARGO_TARGET_DIR is set.
    if let Ok(dir) = env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(dir);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above CARGO_MANIFEST_DIR")
        .join("target")
}

fn make_tmpdir(label: &str) -> PathBuf {
    let mut dir = env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    dir.push(format!("{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("create tmp dir");
    dir
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn write_sine_wav(path: &Path) {
    let spec = WavSpec {
        channels: 2,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::create(path, spec).expect("create sine wav");
    let total_frames = (SAMPLE_RATE as f32 * DURATION_SECS) as usize;
    for frame in 0..total_frames {
        let time_sec = frame as f32 / SAMPLE_RATE as f32;
        let amp = SINE_AMPLITUDE * (2.0 * PI * SINE_FREQ_HZ * time_sec).sin();
        let quantized = (amp * f32::from(i16::MAX)) as i16;
        writer.write_sample(quantized).unwrap();
        writer.write_sample(quantized).unwrap();
    }
    writer.finalize().expect("finalize sine wav");
}

fn write_gain_state(path: &Path, gain_db: f64) {
    let info = Plugin::info();
    let hash = shared_plugin_state_hash(&info);
    let ids = [GainParamsParamId::Gain.as_u32()];
    let values = [gain_db];
    let bytes = serialize_state(hash, &ids, &values, &[]);
    fs::write(path, bytes).expect("write state file");
}

fn run_standalone(bin: &Path, input: &Path, output: &Path, state: Option<&Path>) {
    let mut cmd = Command::new(bin);
    cmd.arg("--headless")
        // Mute device output so dev machines aren't blasted with sine
        // - capture is pre-mute, so the WAV is unaffected.
        .args(["--output-enabled", "off"])
        .args(["--sample-rate", &SAMPLE_RATE.to_string()])
        .arg("--input-file")
        .arg(input)
        .arg("--output-file")
        .arg(output);
    if let Some(state) = state {
        cmd.arg("--state").arg(state);
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
    assert!(
        out.status.success(),
        "standalone exited non-zero: {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        output.is_file(),
        "standalone exited 0 but {} is missing\nstderr:\n{}",
        output.display(),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn read_wav_mono(path: &Path) -> Vec<f32> {
    let mut reader =
        WavReader::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let spec = reader.spec();
    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Int, 16) => reader
            .samples::<i16>()
            .map(|sample| f32::from(sample.unwrap()) / 32768.0)
            .collect(),
        (SampleFormat::Float, 32) => reader.samples::<f32>().map(Result::unwrap).collect(),
        (fmt, bits) => panic!(
            "unexpected WAV format at {}: {fmt:?} {bits}-bit",
            path.display()
        ),
    };
    // Collapse to mono by averaging channels - both channels of the
    // test sine are identical so this is lossless for input, and
    // sums the per-channel gain back to a single number for output
    // (the gain plugin applies the same gain to both channels at
    // pan = 0).
    let channels = spec.channels.max(1) as usize;
    #[allow(clippy::cast_precision_loss)]
    let inv_channels = 1.0_f32 / channels as f32;
    interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() * inv_channels)
        .collect()
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = samples.len() as f32;
    (samples.iter().map(|s| s * s).sum::<f32>() / n).sqrt()
}

/// RMS of the contiguous "active" region of `samples` — frames whose
/// absolute value exceeds `threshold`. Skips both leading silence
/// (cpal warm-up before the audio callback opens) and trailing
/// silence (post-EOF grace samples).
fn active_rms(samples: &[f32], threshold: f32) -> f32 {
    let start = samples
        .iter()
        .position(|s| s.abs() > threshold)
        .unwrap_or(0);
    let end = samples
        .iter()
        .rposition(|s| s.abs() > threshold)
        .map_or(samples.len(), |i| i + 1);
    if end <= start {
        return 0.0;
    }
    rms(&samples[start..end])
}

fn assert_ratio(actual: f32, expected: f32, tolerance: f32, label: &str) {
    let diff = (actual - expected).abs();
    assert!(
        diff <= tolerance,
        "[{label}] RMS ratio {actual:.4} not within ±{tolerance} of {expected:.4} \
         (diff {diff:.4})"
    );
}
