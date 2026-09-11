//! Decode audio files. WAV is always on; other formats need the
//! `symphonia` feature.

use std::path::Path;
use std::sync::Arc;

/// A decoded audio file. `channels` holds one deinterleaved sample
/// buffer per channel. `sample_rate` is the file's native rate; use
/// [`crate::resample()`] to change it.
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    pub channels: Vec<Arc<[f32]>>,
    pub sample_rate: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("failed to read audio file: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported audio format: {0}")]
    UnsupportedFormat(String),
    #[error("corrupt or malformed audio data: {0}")]
    Corrupt(String),
}

impl From<hound::Error> for DecodeError {
    fn from(e: hound::Error) -> Self {
        match e {
            hound::Error::IoError(io) => DecodeError::Io(io),
            hound::Error::FormatError(msg) => DecodeError::Corrupt(msg.to_string()),
            hound::Error::Unsupported => {
                DecodeError::UnsupportedFormat("unsupported WAV encoding".to_string())
            }
            hound::Error::TooWide
            | hound::Error::UnfinishedSample
            | hound::Error::InvalidSampleFormat => DecodeError::Corrupt(e.to_string()),
        }
    }
}

/// Decode an audio file into memory.
///
/// Uses `hound` for `.wav` files (by extension); other formats need
/// the `symphonia` feature.
///
/// # Errors
///
/// [`DecodeError::Io`] if the file cannot be opened or read,
/// [`DecodeError::UnsupportedFormat`] for an unrecognized or
/// unsupported encoding, [`DecodeError::Corrupt`] for malformed data.
pub fn decode_file(path: &Path) -> Result<DecodedAudio, DecodeError> {
    let is_wav = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("wav"));

    if is_wav {
        return decode_wav(path);
    }

    #[cfg(feature = "symphonia")]
    {
        decode_symphonia(path)
    }
    #[cfg(not(feature = "symphonia"))]
    {
        Err(DecodeError::UnsupportedFormat(format!(
            "unrecognized extension for {} - enable the `symphonia` feature for non-WAV formats",
            path.display()
        )))
    }
}

fn decode_wav(path: &Path) -> Result<DecodedAudio, DecodeError> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let num_channels = spec.channels as usize;
    if num_channels == 0 {
        return Err(DecodeError::Corrupt(
            "WAV file declares 0 channels".to_string(),
        ));
    }
    let mut channels: Vec<Vec<f32>> = vec![Vec::new(); num_channels];

    match spec.sample_format {
        hound::SampleFormat::Float => {
            for (i, sample) in reader.samples::<f32>().enumerate() {
                channels[i % num_channels].push(sample?);
            }
        }
        hound::SampleFormat::Int => {
            // hound's int samples are centered on the bit depth
            // (16-bit: -32768 to 32767). Normalize against that depth's amplitude.
            let max_amplitude = 2f64.powi(i32::from(spec.bits_per_sample) - 1);
            for (i, sample) in reader.samples::<i32>().enumerate() {
                #[allow(clippy::cast_possible_truncation)]
                let normalized = (f64::from(sample?) / max_amplitude) as f32;
                channels[i % num_channels].push(normalized);
            }
        }
    }

    Ok(DecodedAudio {
        channels: channels.into_iter().map(Arc::from).collect(),
        sample_rate: f64::from(spec.sample_rate),
    })
}

#[cfg(feature = "symphonia")]
fn decode_symphonia(path: &Path) -> Result<DecodedAudio, DecodeError> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error as SymError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| DecodeError::UnsupportedFormat(e.to_string()))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or_else(|| DecodeError::UnsupportedFormat("no decodable audio track".to_string()))?;
    let track_id = track.id;
    let sample_rate = f64::from(
        track
            .codec_params
            .sample_rate
            .ok_or_else(|| DecodeError::Corrupt("track has no sample rate".to_string()))?,
    );
    let num_channels = track
        .codec_params
        .channels
        .ok_or_else(|| DecodeError::Corrupt("track has no channel layout".to_string()))?
        .count();

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| DecodeError::UnsupportedFormat(e.to_string()))?;

    let mut channels: Vec<Vec<f32>> = vec![Vec::new(); num_channels];
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(DecodeError::Corrupt(e.to_string())),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let frame = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(DecodeError::Corrupt(e.to_string())),
        };
        let buf = sample_buf
            .get_or_insert_with(|| SampleBuffer::new(frame.capacity() as u64, *frame.spec()));
        buf.copy_interleaved_ref(frame);
        for (i, &sample) in buf.samples().iter().enumerate() {
            channels[i % num_channels].push(sample);
        }
    }

    Ok(DecodedAudio {
        channels: channels.into_iter().map(Arc::from).collect(),
        sample_rate,
    })
}
