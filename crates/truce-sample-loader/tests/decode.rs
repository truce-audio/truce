// Rates are exact literals here, not computed values. Counts stay
// small enough that usize -> f64 loses no precision.
#![allow(clippy::float_cmp, clippy::cast_precision_loss)]

use truce_sample_loader::{DecodeError, decode_file, resample};

fn write_test_wav(path: &std::path::Path, sample_rate: u32, channels: u16, samples: &[f32]) {
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    for &s in samples {
        writer.write_sample(s).unwrap();
    }
    writer.finalize().unwrap();
}

#[test]
fn round_trips_mono_wav() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mono.wav");
    let samples = [0.0f32, 0.25, -0.5, 0.75, -1.0];
    write_test_wav(&path, 48_000, 1, &samples);

    let decoded = decode_file(&path).unwrap();
    assert_eq!(decoded.sample_rate, 48_000.0);
    assert_eq!(decoded.channels.len(), 1);
    assert_eq!(decoded.channels[0].len(), samples.len());
    for (a, b) in decoded.channels[0].iter().zip(samples.iter()) {
        assert!((a - b).abs() < 1e-5, "{a} vs {b}");
    }
}

#[test]
fn round_trips_stereo_wav() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stereo.wav");
    // The left and right channels are interleaved.
    let interleaved = [0.1f32, -0.1, 0.2, -0.2, 0.3, -0.3];
    write_test_wav(&path, 44_100, 2, &interleaved);

    let decoded = decode_file(&path).unwrap();
    assert_eq!(decoded.channels.len(), 2);
    assert_eq!(decoded.channels[0].len(), 3);
    assert_eq!(decoded.channels[1].len(), 3);
    assert!((decoded.channels[0][0] - 0.1).abs() < 1e-5);
    assert!((decoded.channels[1][0] - -0.1).abs() < 1e-5);
}

#[test]
fn resample_identity_when_rate_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("id.wav");
    write_test_wav(&path, 48_000, 1, &[0.0, 0.5, -0.5]);

    let decoded = decode_file(&path).unwrap();
    let same_len = decoded.channels[0].len();
    let resampled = resample(decoded, 48_000.0);
    assert_eq!(resampled.sample_rate, 48_000.0);
    assert_eq!(resampled.channels[0].len(), same_len);
}

#[test]
fn resample_scales_output_length() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scale.wav");
    let samples: Vec<f32> = (0..480).map(|i| (i as f32 / 480.0).sin()).collect();
    write_test_wav(&path, 48_000, 1, &samples);

    let decoded = decode_file(&path).unwrap();
    let resampled = resample(decoded, 24_000.0);
    assert_eq!(resampled.sample_rate, 24_000.0);
    // Halving the rate halves the sample count, within a small tolerance.
    let ratio = resampled.channels[0].len() as f64 / 480.0;
    assert!((ratio - 0.5).abs() < 0.01, "ratio={ratio}");
    assert!(resampled.channels[0].iter().all(|s| s.is_finite()));
}

#[test]
fn missing_file_is_err_not_panic() {
    let result = decode_file(std::path::Path::new("/nonexistent/path/does-not-exist.wav"));
    assert!(matches!(result, Err(DecodeError::Io(_))));
}

#[test]
fn corrupt_file_is_err_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.wav");
    std::fs::write(&path, b"not a real wav file, just garbage bytes").unwrap();

    let result = decode_file(&path);
    assert!(
        result.is_err(),
        "corrupt file should not decode successfully"
    );
}

#[cfg(feature = "symphonia")]
mod symphonia_tests {
    use super::*;

    #[test]
    fn unsupported_extension_without_matching_track_errs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.mp3");
        std::fs::write(&path, b"not actually an mp3").unwrap();
        let result = decode_file(&path);
        assert!(result.is_err());
    }
}
