//! [`decode_file`] and [`resample()`] decode and resample audio
//! files for truce plugins, off the audio thread.
//!
//! `hound` decodes WAV by default; the `symphonia` feature adds
//! AIFF, FLAC, MP3, and AAC-in-MP4. [`resample()`] is a separate
//! step so callers control when, and whether, it runs.
//!
//! CAUTION: never call these from `process()`. Both allocate and do
//! I/O, so neither is real-time safe. Call them from a background
//! thread (a truce `BackgroundTask::run`); see
//! `examples/truce-example-ir-loader` for the full
//! decode-then-lock-free-swap pattern.

pub mod decode;
pub mod resample;

pub use decode::{DecodeError, DecodedAudio, decode_file};
pub use resample::resample;
